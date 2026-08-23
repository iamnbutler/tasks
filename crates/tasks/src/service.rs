//! `tasks service` — the server as a launchd-managed service.
//!
//! The OrbStack/Tailscale shape: the daemon is the product, launchd owns its
//! lifecycle, and every client — the gpui app included — is just a client.
//! `install` copies this binary to a stable home (`~/.tasks/bin/tasks` —
//! never an app bundle, never `target/`, both of which are homes something
//! else deletes), writes one LaunchAgent, and hands the rest to launchd:
//! start at login, restart on crash. There is deliberately **no second
//! LaunchAgent for vm-pool** — the server's autospawn
//! ([`crate::run::Config::vm_pool_autospawn`]) already supervises the pool
//! (failed connect → respawn), keeps it detached so it outlives server
//! restarts, and the pool's refuse-if-listening guard keeps ownership
//! unambiguous. One agent, two daemons.
//!
//! Three decisions here are more load-bearing than they look:
//!
//! - **`KeepAlive` is unconditional, so a plain SIGTERM is a restart, not a
//!   stop.** That is what "service" means — a crash comes back, login brings
//!   it up — and it is why [`crate::reload`] must *delegate* to launchctl
//!   when the serving binary is the managed one: `tasks stop`'s SIGTERM
//!   would report "stopped" while launchd resurrects the server behind the
//!   report. Stopping a managed server is `bootout`; replacing it is
//!   `kickstart -k`.
//! - **The boot mode is the plist's, and a carried mode arrives by `POST`
//!   after the swap is verified.** An unmanaged reload carries the mode in
//!   the child's environment; launchd owns the managed child's environment,
//!   and pinning a carried mode into the plist would change what a *crash*
//!   restart boots into — the boot-comes-up-quiet rule exists precisely for
//!   restarts nobody asked for. So the plist keeps the configured default
//!   (`pause` unless `--default-mode play` says otherwise), which makes the
//!   window between boot and the carry `POST` quiet — the safe direction —
//!   and the one configuration where it is not (`--default-mode play`, with
//!   `pause` to carry) is printed when it happens rather than left to be
//!   discovered.
//! - **`install` is idempotent and is also the upgrade.** Copy the binary
//!   (write-then-rename beside the destination — overwriting a running,
//!   signed executable in place is how macOS kills the process mid-write),
//!   rewrite the plist, `bootout` + `bootstrap`, verify against `/status` on
//!   the *new* pid. Running it from the app's bundled seed, from a checkout,
//!   or from `~/.tasks/bin` itself all mean the same thing: make the service
//!   serve this binary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tasks_api::models::Mode;
use thiserror::Error;
use tokio::process::Command;

use crate::pidfile;

/// The LaunchAgent label, and the plist's file stem.
pub const LABEL: &str = "com.iamnbutler.tasks.server";

/// The service home under `$HOME`. The binary lives at `<home>/bin/tasks`.
pub const HOME_DIR: &str = ".tasks";

/// How long a fresh boot gets to publish a pidfile and answer `/status`.
const START_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("HOME is not set; there is nowhere to install a per-user service")]
    NoHome,
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("launchctl {verb} failed ({status}): {stderr}")]
    Launchctl {
        verb: &'static str,
        status: String,
        stderr: String,
    },
    #[error("no service is installed (no {0}); `tasks service install` creates it")]
    NotInstalled(PathBuf),
    #[error("{0}")]
    Other(String),
}

fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> ServiceError {
    let context = context.into();
    move |source| ServiceError::Io { context, source }
}

/// Where the pieces of the managed service live. Everything hangs off
/// `$HOME`, resolved once, so the CLI, the reload delegation and the tests
/// cannot disagree about which service they are talking about.
#[derive(Debug, Clone, PartialEq)]
pub struct ServicePaths {
    /// The `$HOME` everything below hangs off — kept because the plist's
    /// `PATH` needs it too (per-user tool dirs, [`plist_contents`]).
    pub home: PathBuf,
    /// `~/.tasks/bin/tasks` — the binary's stable home.
    pub bin: PathBuf,
    /// `~/Library/LaunchAgents/<LABEL>.plist`.
    pub plist: PathBuf,
}

impl ServicePaths {
    /// From `$HOME`. `None` only in a homeless environment.
    pub fn resolve() -> Result<Self, ServiceError> {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(|home| Self::under(Path::new(&home)))
            .ok_or(ServiceError::NoHome)
    }

    /// The layout rule, testable without an environment.
    pub fn under(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            bin: home.join(HOME_DIR).join("bin").join("tasks"),
            plist: home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
        }
    }

    /// A service is installed — the plist exists. Says nothing about whether
    /// it is loaded or serving; those are launchd's and the port's to answer.
    pub fn installed(&self) -> bool {
        self.plist.is_file()
    }
}

/// Whether the server `reload`/`stop` are about to act on is the managed one
/// — the fact that decides delegation to launchctl.
///
/// Three tests, all load-bearing. No plist means no service, however the
/// binary got where it is. **The agent's pinned `TASKS_DATA_DIR` must be the
/// data dir in hand** — the service's identity is its data dir, so a reload
/// pointed anywhere else (every test's tempdir, a second deployment) is
/// about a *different* server and must never reach for the operator's real
/// launchd session. And a pidfile naming some other binary means a developer
/// is serving beside the service (a `target/debug/tasks` on another port);
/// their `make restart` must keep meaning what it always meant, so only a
/// pidfile that names the service's own binary — or no pidfile at all —
/// reads as managed.
pub fn managed(data_dir: &Path) -> Option<ServicePaths> {
    let paths = ServicePaths::resolve().ok()?;
    if !paths.installed() {
        return None;
    }
    let contents = std::fs::read_to_string(&paths.plist).ok()?;
    let pinned = plist_data_dir(&contents)?;
    if !same_path(&pinned, data_dir) {
        return None;
    }
    match pidfile::read_live(data_dir) {
        None => Some(paths),
        Some(file) => (file.exe == paths.bin).then_some(paths),
    }
}

/// Path equality for the delegation decision: canonical when both resolve,
/// literal otherwise. Canonicalization is what forgives a symlinked `$HOME`
/// or a trailing slash; the literal fallback is what keeps a data dir that
/// does not exist yet comparable at all.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Whether a `tasks` typed at a terminal is the one the service runs.
///
/// `install` puts the binary at `~/.tasks/bin/tasks` — the right stable home,
/// and on nobody's `PATH` — so every CLI verb this repository documents still
/// needs a checkout or a typed path on an installed machine (#991). This is
/// the diagnosis; the words are [`path_advice_lines`]'.
///
/// The case worth the code is [`PathAdvice::Shadowed`]: a checkout's
/// `target/debug/tasks`, a stale copy or an older install means `tasks status`
/// answers confidently about a **different** binary than the service is
/// running. That is a wrong answer rather than a missing one, and it is the
/// only one of the three states that fails quietly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathAdvice {
    /// The installed binary is what a shell would run — however it got there.
    /// A `/usr/local/bin/tasks -> ~/.tasks/bin/tasks` symlink, and the brew
    /// route (#997), land here, because the comparison canonicalizes: this
    /// must accommodate the other routes, not compete with them.
    Resolved,
    /// No `tasks` on the `PATH` at all. Fails loudly on its own.
    Absent(PathSuggestion),
    /// A different `tasks` wins the search.
    Shadowed {
        /// What a shell would actually run.
        other: PathBuf,
        suggestion: PathSuggestion,
    },
}

/// The one line to paste, and where it goes.
///
/// Derived from `$SHELL`'s basename rather than written once for every shell:
/// `export PATH=` is not valid in fish, so a single portable line would be
/// advice that cannot be followed on the shell that needs it most.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSuggestion {
    /// The line itself. Spells `$HOME/.tasks/bin` when the directory is under
    /// `$HOME` — a line to paste rather than one to translate, and one that
    /// survives a home that moves.
    pub line: String,
    /// The file it goes in, or `None` when we could not tell which shell this
    /// is. A **guessed** rc file sends somebody editing a file their shell
    /// never sources, which is worse than silence.
    pub file: Option<String>,
}

/// The advice, computed from parameters and nothing else.
///
/// Every input is an argument — no environment read inside — so it is testable
/// the way [`plist_contents`] and [`same_path`] are, and so the caller decides
/// *whose* environment is being judged. That last part matters: `service
/// install` is also run by the **app**, whose parent is launchd, whose `PATH`
/// is not the terminal's.
pub fn path_advice(bin: &Path, path_var: &str, shell: Option<&str>, home: &Path) -> PathAdvice {
    let found = resolve_on_path(path_var);
    match found {
        Some(other) if same_path(&other, bin) => PathAdvice::Resolved,
        Some(other) => PathAdvice::Shadowed {
            other,
            suggestion: suggest(bin, shell, home),
        },
        None => PathAdvice::Absent(suggest(bin, shell, home)),
    }
}

/// The first `tasks` a shell would run, walking `PATH` in order.
///
/// **The first, and an executable one.** A shell searching `PATH` skips an
/// entry that exists but is not executable and keeps looking, so testing for a
/// mere file would report a stray download or a half-copied file as a shadow
/// when nothing is actually shadowed — a false accusation in the one state
/// this code exists for. And a later correct entry does not rescue an earlier
/// wrong one, because that is not what execution does.
fn resolve_on_path(path_var: &str) -> Option<PathBuf> {
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join("tasks"))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn suggest(bin: &Path, shell: Option<&str>, home: &Path) -> PathSuggestion {
    let dir = bin.parent().unwrap_or(bin);
    // `$HOME/.tasks/bin` rather than `/Users/nate/.tasks/bin`: a line to
    // paste, and one that survives a home that moves.
    let spelled = match dir.strip_prefix(home) {
        Ok(rest) => format!("$HOME/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    };
    let basename = shell
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match basename {
        // `export PATH=` is not valid fish, so this is the one shell that
        // needs a different line rather than a different file.
        "fish" => PathSuggestion {
            line: format!("fish_add_path {spelled}"),
            file: Some("~/.config/fish/config.fish".into()),
        },
        "zsh" => PathSuggestion {
            line: format!("export PATH=\"{spelled}:$PATH\""),
            file: Some("~/.zshrc".into()),
        },
        // A macOS terminal window is a login shell, so `.bash_profile` and
        // not `.bashrc`.
        "bash" => PathSuggestion {
            line: format!("export PATH=\"{spelled}:$PATH\""),
            file: Some("~/.bash_profile".into()),
        },
        _ => PathSuggestion {
            line: format!("export PATH=\"{spelled}:$PATH\""),
            file: None,
        },
    }
}

/// The advice rendered against *this* process's environment — **empty on
/// [`PathAdvice::Resolved`]**, so a machine that is already set up says
/// nothing at all.
///
/// Printed rather than acted on, deliberately. `service install` is also run
/// by the app, whose parent is launchd and whose `PATH` is not the terminal's,
/// so a spurious suggestion costs a duplicated entry while *acting* would mean
/// writing to a shell profile on evidence that is not about that shell.
pub fn path_advice_lines(bin: &Path, home: &Path) -> String {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let shell = std::env::var("SHELL").ok();
    render_path_advice(&path_advice(bin, &path_var, shell.as_deref(), home), bin)
}

/// The words. Split from [`path_advice`] so the decision is testable without
/// asserting on prose, and the prose without an environment.
pub fn render_path_advice(advice: &PathAdvice, bin: &Path) -> String {
    let (headline, suggestion) = match advice {
        PathAdvice::Resolved => return String::new(),
        PathAdvice::Absent(suggestion) => (
            format!(
                "`tasks` is installed at {} and is not on your PATH, so typing \
                 `tasks` in a terminal will not find it.",
                bin.display()
            ),
            suggestion,
        ),
        // Both paths, because "add this line" alone does not explain why the
        // numbers disagreed.
        PathAdvice::Shadowed { other, suggestion } => (
            format!(
                "A different `tasks` is earlier on your PATH ({}), so typed commands \
                 answer about it rather than about {} — the binary this service runs.",
                other.display(),
                bin.display()
            ),
            suggestion,
        ),
    };
    let mut out = format!(
        "\n{headline}\nAdd it to your PATH:\n\n    {}\n",
        suggestion.line
    );
    if let Some(file) = &suggestion.file {
        out.push_str(&format!("\n(in {file}, then open a new terminal)\n"));
    }
    out
}

/// The `TASKS_DATA_DIR` an installed agent pins — read back out of our own
/// plist, whose shape [`plist_contents`] owns, so there is no second parser
/// to drift. `None` for a plist this code did not write, which reads as
/// "not ours to delegate to".
pub fn plist_data_dir(contents: &str) -> Option<PathBuf> {
    let after_key = contents.split("<key>TASKS_DATA_DIR</key>").nth(1)?;
    let value = after_key
        .split("<string>")
        .nth(1)?
        .split("</string>")
        .next()?;
    Some(PathBuf::from(xml_unescape(value)))
}

/// The reverse of [`xml_escape`]. `&amp;` last, so an escaped ampersand
/// cannot cascade into a second round of unescaping.
fn xml_unescape(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The LaunchAgent, rendered. Pure, so the tests can pin the shape without a
/// Mac in the loop.
///
/// `TASKS_DEFAULT_MODE` appears only when the operator chose one: absent, the
/// server's own default (`pause`) applies, and a crash restart comes back
/// quiet — writing `pause` explicitly would be the same behaviour today and
/// a stale pin the day the server's default moves.
///
/// The `PATH` is launchd's minimal one plus the usual toolchain locations,
/// for the same reason the app prepends them: the server shells out (`git`
/// for landings), and launchd's `PATH` finds tools for exactly the people
/// who did not need the help. The per-user dirs lead it because they are
/// where the tools this server actually spawns live: `~/.local/bin` is the
/// `claude` native installer's home — under launchd the orchestrator's
/// `spawn agent: No such file or directory` was exactly this entry missing —
/// and `~/.cargo/bin` is what an orchestrator pointed at a checkout builds
/// with.
pub fn plist_contents(
    home: &Path,
    bin: &Path,
    data_dir: &Path,
    log: &Path,
    default_mode: Option<Mode>,
) -> String {
    let mode_entry = match default_mode {
        Some(mode) => format!(
            "\n\t\t<key>TASKS_DEFAULT_MODE</key>\n\t\t<string>{}</string>",
            mode.as_str()
        ),
        None => String::new(),
    };
    let path_env = format!(
        "{home}/.local/bin:{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        home = home.display()
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{bin}</string>
		<string>serve</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>TASKS_DATA_DIR</key>
		<string>{data_dir}</string>
		<key>PATH</key>
		<string>{path_env}</string>{mode_entry}
	</dict>
</dict>
</plist>
"#,
        label = LABEL,
        bin = xml_escape(&bin.display().to_string()),
        log = xml_escape(&log.display().to_string()),
        data_dir = xml_escape(&data_dir.display().to_string()),
        path_env = xml_escape(&path_env),
    )
}

/// The five characters XML cares about. Paths are the only interpolated
/// values, and a path with `&` in it is rare — which is exactly why an
/// unescaped one would survive until the worst moment.
fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Put `source` at `paths.bin`, atomically, unless it already is that file.
///
/// Write-then-rename beside the destination: renaming over a running
/// executable leaves the old inode to the running process, where overwriting
/// in place invalidates the running image's code signature and macOS kills
/// it. "Already that file" is decided on canonical paths, so
/// `~/.tasks/bin/tasks service install` (a re-register) skips the copy
/// rather than truncating the binary it is running from.
pub fn install_binary(source: &Path, paths: &ServicePaths) -> Result<bool, ServiceError> {
    let dir = paths
        .bin
        .parent()
        .ok_or_else(|| ServiceError::Other(format!("no parent for {}", paths.bin.display())))?;
    std::fs::create_dir_all(dir).map_err(io_err(format!("creating {}", dir.display())))?;

    let source_canon = source
        .canonicalize()
        .map_err(io_err(format!("resolving {}", source.display())))?;
    if let Ok(dest_canon) = paths.bin.canonicalize()
        && source_canon == dest_canon
    {
        return Ok(false);
    }

    let staging = dir.join(".tasks.new");
    std::fs::copy(&source_canon, &staging)
        .map_err(io_err(format!("copying to {}", staging.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
            .map_err(io_err("marking the staged binary executable"))?;
    }
    std::fs::rename(&staging, &paths.bin)
        .map_err(io_err(format!("renaming into {}", paths.bin.display())))?;
    Ok(true)
}

/// Write the LaunchAgent. `0644` and a plain write: launchd re-reads it at
/// bootstrap, so there is no running reader to race.
pub fn write_plist(
    paths: &ServicePaths,
    data_dir: &Path,
    default_mode: Option<Mode>,
) -> Result<(), ServiceError> {
    let dir = paths
        .plist
        .parent()
        .ok_or_else(|| ServiceError::Other(format!("no parent for {}", paths.plist.display())))?;
    std::fs::create_dir_all(dir).map_err(io_err(format!("creating {}", dir.display())))?;
    let log = tasks_api::paths::serve_log(data_dir);
    std::fs::write(
        &paths.plist,
        plist_contents(&paths.home, &paths.bin, data_dir, &log, default_mode),
    )
    .map_err(io_err(format!("writing {}", paths.plist.display())))
}

// --- launchctl ---

/// `gui/<uid>` — the per-user launchd domain. Read from `id -u` rather than
/// libc, which this crate does not otherwise link.
async fn gui_domain() -> Result<String, ServiceError> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .await
        .map_err(io_err("running id -u"))?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || uid.is_empty() {
        return Err(ServiceError::Other(
            "could not read the uid from `id -u`".into(),
        ));
    }
    Ok(format!("gui/{uid}"))
}

async fn launchctl(verb: &'static str, args: &[&str]) -> Result<(), ServiceError> {
    let output = Command::new("launchctl")
        .arg(verb)
        .args(args)
        .output()
        .await
        .map_err(io_err(format!("running launchctl {verb}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(ServiceError::Launchctl {
        verb,
        status: output.status.to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

/// Whether launchd currently knows the job. `launchctl print` on the service
/// target: exit 0 is loaded, anything else is not — the distinction `start`
/// and `restart` branch on.
pub async fn loaded() -> Result<bool, ServiceError> {
    let domain = gui_domain().await?;
    let output = Command::new("launchctl")
        .args(["print", &format!("{domain}/{LABEL}")])
        .output()
        .await
        .map_err(io_err("running launchctl print"))?;
    Ok(output.status.success())
}

/// Load the plist. `bootout` first, ignoring its failure: a job that is
/// already loaded holds the *old* plist, and bootstrap refuses a loaded
/// label, so the sequence is what makes `install` idempotent.
///
/// The bootstrap itself retries, because `bootout` returning is not the job
/// being gone: launchd tears it down asynchronously, and a bootstrap that
/// lands inside the teardown fails with exit 5 ("Input/output error") — which
/// is exactly how the very first real `service install` on a machine with a
/// running service failed. The retry is bounded and only for failure, so a
/// genuinely broken plist still surfaces its error, just a few seconds later.
pub async fn bootstrap(paths: &ServicePaths) -> Result<(), ServiceError> {
    let domain = gui_domain().await?;
    let _ = launchctl("bootout", &[&format!("{domain}/{LABEL}")]).await;
    let plist = paths.plist.display().to_string();
    let mut last = Err(ServiceError::Other("bootstrap never attempted".into()));
    for attempt in 0..10 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        last = launchctl("bootstrap", &[&domain, &plist]).await;
        if last.is_ok() {
            return last;
        }
    }
    last
}

/// Unload the job — the only stop that sticks, given `KeepAlive`.
pub async fn bootout() -> Result<(), ServiceError> {
    let domain = gui_domain().await?;
    launchctl("bootout", &[&format!("{domain}/{LABEL}")]).await
}

/// `launchctl kickstart` on the loaded job: with `kill`, replace a running
/// server (the restart primitive); without, start an idle job and leave a
/// running one alone (the start primitive).
pub async fn launchctl_kickstart(kill: bool) -> Result<(), ServiceError> {
    let domain = gui_domain().await?;
    let target = format!("{domain}/{LABEL}");
    let args: Vec<&str> = match kill {
        true => vec!["-k", &target],
        false => vec![&target],
    };
    launchctl("kickstart", &args).await
}

// --- the composite ops `tasks service` exposes ---

/// What `install` did, for the caller to report.
pub struct InstallOutcome {
    /// Whether a binary was copied into the home (false: re-registered the
    /// one already there).
    pub copied: bool,
    /// The server that came up.
    pub file: pidfile::PidFile,
    pub paths: ServicePaths,
}

/// Install (or upgrade) the service from **this** binary: copy it home,
/// write the LaunchAgent, load it, and wait for a server to answer. The
/// one-button path — the app's bundled seed, a checkout, and the installed
/// binary itself all run the same thing.
pub async fn install(
    data_dir: &Path,
    default_mode: Option<Mode>,
) -> Result<InstallOutcome, ServiceError> {
    let paths = ServicePaths::resolve()?;
    let source = std::env::current_exe().map_err(io_err("resolving this executable"))?;
    let previous = pidfile::read_live(data_dir).map(|f| f.pid);
    let copied = install_binary(&source, &paths)?;
    write_plist(&paths, data_dir, default_mode)?;
    bootstrap(&paths).await?;
    let file = wait_for_serving(data_dir, previous).await?;
    Ok(InstallOutcome {
        copied,
        file,
        paths,
    })
}

/// Load the service and wait for it to serve. Refuses when nothing is
/// installed — a start cannot invent a plist without also deciding where the
/// binary lives, and that decision is `install`'s.
pub async fn start(data_dir: &Path) -> Result<pidfile::PidFile, ServiceError> {
    let paths = ServicePaths::resolve()?;
    if !paths.installed() {
        return Err(ServiceError::NotInstalled(paths.plist));
    }
    let previous = pidfile::read_live(data_dir).map(|f| f.pid);
    match loaded().await? {
        // kickstart without -k starts a loaded-but-idle job and leaves a
        // running one alone — both of which are what "start" means.
        true => launchctl_kickstart(false).await?,
        false => bootstrap(&paths).await?,
    }
    wait_for_serving(data_dir, previous).await
}

/// Unload the service — the stop that sticks under `KeepAlive`. Returns what
/// was serving, if anything. The job loads again at the next login
/// (`RunAtLoad`); `uninstall` is the durable off.
pub async fn stop(data_dir: &Path) -> Result<Option<pidfile::PidFile>, ServiceError> {
    let paths = ServicePaths::resolve()?;
    if !paths.installed() {
        return Err(ServiceError::NotInstalled(paths.plist));
    }
    let serving = pidfile::read_live(data_dir).filter(|f| f.exe == paths.bin);
    if !loaded().await? {
        return Ok(None);
    }
    bootout().await?;
    if let Some(file) = &serving {
        if !crate::reload::wait_gone(file.pid, Duration::from_secs(20)).await {
            return Err(ServiceError::Other(format!(
                "the job was unloaded but pid {} is still running",
                file.pid
            )));
        }
        pidfile::remove_if_ours(data_dir, file.pid);
    }
    Ok(serving)
}

/// Kill-and-relaunch (or load, if unloaded) and wait for the new server.
pub async fn restart(data_dir: &Path) -> Result<pidfile::PidFile, ServiceError> {
    let paths = ServicePaths::resolve()?;
    if !paths.installed() {
        return Err(ServiceError::NotInstalled(paths.plist));
    }
    let previous = pidfile::read_live(data_dir).map(|f| f.pid);
    match loaded().await? {
        true => launchctl_kickstart(true).await?,
        false => bootstrap(&paths).await?,
    }
    wait_for_serving(data_dir, previous).await
}

/// Unload and remove the LaunchAgent. The binary and the data dir stay —
/// deleting work product is never a side effect of removing a supervisor.
pub async fn uninstall() -> Result<ServicePaths, ServiceError> {
    let paths = ServicePaths::resolve()?;
    if !paths.installed() {
        return Err(ServiceError::NotInstalled(paths.plist));
    }
    let _ = bootout().await;
    std::fs::remove_file(&paths.plist)
        .map_err(io_err(format!("removing {}", paths.plist.display())))?;
    Ok(paths)
}

/// The service's standing state, one line per fact, for `tasks service
/// status` — which follows it with the serving report the other status
/// surfaces already share.
pub async fn status_lines() -> Result<String, ServiceError> {
    let paths = ServicePaths::resolve()?;
    let mut out = String::new();
    out.push_str(&format!(
        "agent:   {} ({})\n",
        paths.plist.display(),
        if paths.installed() {
            "installed"
        } else {
            "not installed"
        }
    ));
    out.push_str(&format!(
        "binary:  {} ({})\n",
        paths.bin.display(),
        if paths.bin.is_file() {
            "present"
        } else {
            "missing"
        }
    ));
    if paths.installed() {
        out.push_str(&format!(
            "launchd: {}\n",
            if loaded().await? {
                "loaded"
            } else {
                "not loaded (tasks service start)"
            }
        ));
    }
    // Where a human asks "is this set up" — and silent when it is.
    out.push_str(&path_advice_lines(&paths.bin, &paths.home));
    Ok(out)
}

/// Wait for a serving pid other than `previous` to publish itself and answer
/// `/status`. The verification half of every managed swap: launchd reported
/// nothing when it relaunched, so the proof has to come from the server.
pub async fn wait_for_serving(
    data_dir: &Path,
    previous: Option<u32>,
) -> Result<pidfile::PidFile, ServiceError> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Some(file) = pidfile::read_live(data_dir)
            && Some(file.pid) != previous
            && crate::reload::fetch_status(file.port).await.is_ok()
        {
            return Ok(file);
        }
        if tokio::time::Instant::now() >= deadline {
            let log = tasks_api::paths::serve_log(data_dir);
            return Err(ServiceError::Other(format!(
                "no new server answered /status within {}s; tail of {}:\n{}",
                START_TIMEOUT.as_secs(),
                log.display(),
                crate::reload::log_tail(&log),
            )));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod path_advice_tests {
    use super::*;

    /// Write an executable `tasks` into `dir` and return it.
    fn plant(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("tasks");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Silent when the installed binary is what a shell would run — and the
    /// symlink route (`/usr/local/bin/tasks -> ~/.tasks/bin/tasks`, which is
    /// also what a brew formula would leave) counts, because the comparison
    /// canonicalizes. This must accommodate the other routes, not compete
    /// with them.
    #[test]
    fn an_installed_binary_on_the_path_is_silent_however_it_got_there() {
        let home = tempfile::tempdir().unwrap();
        let paths = ServicePaths::under(home.path());
        plant(paths.bin.parent().unwrap());

        let direct = paths.bin.parent().unwrap().display().to_string();
        let advice = path_advice(&paths.bin, &direct, Some("/bin/zsh"), home.path());
        assert_eq!(advice, PathAdvice::Resolved);
        assert_eq!(render_path_advice(&advice, &paths.bin), "");

        // …and through a symlink somebody else's route put there.
        #[cfg(unix)]
        {
            let local = home.path().join("usr-local-bin");
            std::fs::create_dir_all(&local).unwrap();
            std::os::unix::fs::symlink(&paths.bin, local.join("tasks")).unwrap();
            let via_link = format!("{}:{direct}", local.display());
            assert_eq!(
                path_advice(&paths.bin, &via_link, Some("/bin/zsh"), home.path()),
                PathAdvice::Resolved,
                "a symlink to the installed binary is the installed binary"
            );
        }
    }

    /// Absent gets a line and, where the shell is known, a file. An unknown
    /// shell gets the portable line and **no file**: a guessed rc file sends
    /// somebody editing a file their shell never sources.
    #[test]
    fn an_absent_tasks_suggests_a_line_and_a_file_per_shell() {
        let home = tempfile::tempdir().unwrap();
        let paths = ServicePaths::under(home.path());

        let PathAdvice::Absent(zsh) = path_advice(&paths.bin, "", Some("/bin/zsh"), home.path())
        else {
            panic!("nothing on an empty PATH");
        };
        assert_eq!(zsh.line, "export PATH=\"$HOME/.tasks/bin:$PATH\"");
        assert_eq!(zsh.file.as_deref(), Some("~/.zshrc"));

        // fish cannot take `export PATH=`, so one line for every shell would
        // be advice that cannot be followed.
        let PathAdvice::Absent(fish) =
            path_advice(&paths.bin, "", Some("/opt/homebrew/bin/fish"), home.path())
        else {
            panic!("nothing on an empty PATH");
        };
        assert_eq!(fish.line, "fish_add_path $HOME/.tasks/bin");
        assert_eq!(fish.file.as_deref(), Some("~/.config/fish/config.fish"));

        let PathAdvice::Absent(unknown) = path_advice(&paths.bin, "", None, home.path()) else {
            panic!("nothing on an empty PATH");
        };
        assert!(unknown.line.starts_with("export PATH="));
        assert_eq!(unknown.file, None, "never guess an rc file");
    }

    /// The case worth the code: a *different* `tasks` answers confidently
    /// about a binary the service is not running. The message names both
    /// paths, because "add this line" alone does not explain why the numbers
    /// disagreed.
    #[test]
    fn another_tasks_earlier_on_the_path_is_a_shadow_not_an_absence() {
        let home = tempfile::tempdir().unwrap();
        let paths = ServicePaths::under(home.path());
        plant(paths.bin.parent().unwrap());
        let checkout = home.path().join("src/tasks/target/debug");
        let stale = plant(&checkout);

        let path_var = format!(
            "{}:{}",
            checkout.display(),
            paths.bin.parent().unwrap().display()
        );
        let advice = path_advice(&paths.bin, &path_var, Some("/bin/zsh"), home.path());
        let PathAdvice::Shadowed { other, .. } = &advice else {
            panic!("the first hit wins, and a later correct entry does not rescue it");
        };
        assert!(same_path(other, &stale));

        let words = render_path_advice(&advice, &paths.bin);
        assert!(words.contains("target/debug"), "{words}");
        assert!(words.contains(".tasks/bin"), "{words}");
    }

    /// A `tasks` that exists and is not executable is skipped, exactly as a
    /// shell skips it — so a stray download or a half-copied file is not
    /// reported as a shadow, which would be a false accusation in the one
    /// state this code exists for.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_tasks_is_not_a_shadow() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let paths = ServicePaths::under(home.path());
        plant(paths.bin.parent().unwrap());

        let downloads = home.path().join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let dud = downloads.join("tasks");
        std::fs::write(&dud, b"half a file").unwrap();
        std::fs::set_permissions(&dud, std::fs::Permissions::from_mode(0o644)).unwrap();

        let path_var = format!(
            "{}:{}",
            downloads.display(),
            paths.bin.parent().unwrap().display()
        );
        assert_eq!(
            path_advice(&paths.bin, &path_var, Some("/bin/zsh"), home.path()),
            PathAdvice::Resolved,
            "a shell skips a non-executable entry and keeps looking"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_hangs_off_home() {
        let paths = ServicePaths::under(Path::new("/Users/nb"));
        assert_eq!(paths.bin, PathBuf::from("/Users/nb/.tasks/bin/tasks"));
        assert_eq!(
            paths.plist,
            PathBuf::from("/Users/nb/Library/LaunchAgents/com.iamnbutler.tasks.server.plist")
        );
    }

    /// The plist is the whole contract with launchd; pin the parts that are
    /// load-bearing rather than the byte-for-byte rendering.
    #[test]
    fn the_plist_carries_the_contract() {
        let contents = plist_contents(
            Path::new("/Users/nb"),
            Path::new("/Users/nb/.tasks/bin/tasks"),
            Path::new("/Users/nb/.local/state/tasks-v2"),
            Path::new("/Users/nb/.local/state/tasks-v2/serve.log"),
            None,
        );
        assert!(contents.contains("<string>com.iamnbutler.tasks.server</string>"));
        assert!(contents.contains("<string>/Users/nb/.tasks/bin/tasks</string>"));
        assert!(contents.contains("<string>serve</string>"));
        // KeepAlive is what makes SIGTERM a restart — the fact the reload
        // delegation exists for. If this moves, that reasoning moves.
        assert!(contents.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(contents.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(contents.contains("<key>TASKS_DATA_DIR</key>"));
        // The per-user tool dirs lead the PATH: `~/.local/bin` is where the
        // `claude` native installer puts the binary the orchestrator spawns,
        // and under launchd there is no shell profile to find it with.
        assert!(
            contents
                .contains("<string>/Users/nb/.local/bin:/Users/nb/.cargo/bin:/opt/homebrew/bin")
        );
        // No operator choice, no pin: a crash restart boots the server's own
        // quiet default.
        assert!(!contents.contains("TASKS_DEFAULT_MODE"));
    }

    #[test]
    fn a_chosen_default_mode_is_pinned_and_only_then() {
        let contents = plist_contents(
            Path::new("/t"),
            Path::new("/t/tasks"),
            Path::new("/t/data"),
            Path::new("/t/data/serve.log"),
            Some(Mode::Play),
        );
        assert!(contents.contains("<key>TASKS_DEFAULT_MODE</key>"));
        assert!(contents.contains("<string>play</string>"));
    }

    /// The delegation guard's first line of defence: the data dir pinned at
    /// install reads back out of the plist exactly, escapes included. A
    /// mismatch — every test tempdir, every second deployment — is what
    /// keeps a reload from reaching for the operator's real launchd session.
    #[test]
    fn the_pinned_data_dir_roundtrips_and_gates_delegation() {
        let data_dir = Path::new("/Users/a&b/.local/state/tasks-v2");
        let contents = plist_contents(
            Path::new("/t"),
            Path::new("/t/tasks"),
            data_dir,
            Path::new("/t/serve.log"),
            None,
        );
        assert_eq!(plist_data_dir(&contents).as_deref(), Some(data_dir));
        assert!(!same_path(
            &plist_data_dir(&contents).unwrap(),
            Path::new("/some/tempdir/data")
        ));

        // A plist this code did not write pins nothing, and nothing is the
        // answer that never delegates.
        assert_eq!(plist_data_dir("<plist><dict/></plist>"), None);
    }

    #[test]
    fn paths_are_xml_escaped() {
        let contents = plist_contents(
            Path::new("/Users/a&b"),
            Path::new("/Users/a&b/.tasks/bin/tasks"),
            Path::new("/Users/a&b/data"),
            Path::new("/Users/a&b/data/serve.log"),
            None,
        );
        assert!(contents.contains("/Users/a&amp;b/.tasks/bin/tasks"));
        // The PATH interpolates $HOME too, so it escapes with everything else.
        assert!(contents.contains("/Users/a&amp;b/.local/bin"));
        assert!(!contents.contains("a&b"));
    }

    /// Install is the upgrade: a different binary lands, the same binary is a
    /// no-op copy — and never a truncation of the file it is running from.
    #[test]
    fn install_binary_copies_once_and_recognises_itself() {
        let home = tempfile::tempdir().unwrap();
        let paths = ServicePaths::under(home.path());
        let source = home.path().join("seed-tasks");
        std::fs::write(&source, b"binary bytes").unwrap();

        assert!(install_binary(&source, &paths).unwrap());
        assert_eq!(std::fs::read(&paths.bin).unwrap(), b"binary bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&paths.bin).unwrap().permissions().mode();
            assert_eq!(mode & 0o755, 0o755);
        }

        // Re-installing from the installed binary itself: no copy.
        assert!(!install_binary(&paths.bin, &paths).unwrap());
        assert_eq!(std::fs::read(&paths.bin).unwrap(), b"binary bytes");

        // A new build replaces it.
        std::fs::write(&source, b"newer bytes").unwrap();
        assert!(install_binary(&source, &paths).unwrap());
        assert_eq!(std::fs::read(&paths.bin).unwrap(), b"newer bytes");
    }

    /// The delegation predicate: a plist alone is not enough when a developer
    /// is serving their own binary beside the service.
    #[test]
    fn managed_requires_the_serving_binary_to_be_the_services() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        // Point HOME resolution at the temp home via under(); managed() reads
        // the real environment, so exercise the pieces it is made of instead.
        let paths = ServicePaths::under(home.path());
        assert!(!paths.installed());

        std::fs::create_dir_all(paths.plist.parent().unwrap()).unwrap();
        std::fs::write(&paths.plist, "plist").unwrap();
        assert!(paths.installed());

        // No pidfile: managed (the service may simply be stopped).
        assert!(pidfile::read_live(data_dir.path()).is_none());

        // A pidfile naming a different binary: the developer's server, not
        // ours to delegate.
        let file = pidfile::PidFile {
            pid: std::process::id(),
            port: 4800,
            started_at: chrono::Utc::now(),
            exe: PathBuf::from("/w/tasks/target/debug/tasks"),
        };
        std::fs::write(
            tasks_api::paths::pid_file(data_dir.path()),
            serde_json::to_string(&file).unwrap(),
        )
        .unwrap();
        let live = pidfile::read_live(data_dir.path()).unwrap();
        assert_ne!(live.exe, paths.bin);
    }
}
