//! Session supervisor — runs as PID 1 inside containers.
//!
//! Manages the agent process lifecycle and bridges communication between
//! the host and the agent via the JSON-lines supervisor protocol.
//! See spec/session-runtime.md §4.

use std::io::{self, BufRead, Write};
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Protocol types (matching spec/session-runtime.md §4)
// ---------------------------------------------------------------------------

/// Commands from host → supervisor (§4.1).
#[derive(serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Cmd {
    Start {
        repo: String,
        branch: String,
        prompt: String,
    },
    Chat {
        text: String,
    },
    Stop,
    Exec {
        id: String,
        argv: Vec<String>,
    },
}

/// Events from supervisor → host (§4.2).
/// Serialized as single-line JSON to stdout.
#[derive(serde::Serialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
enum Ev {
    #[serde(rename = "system:ready")]
    SystemReady {},
    #[serde(rename = "agent:started")]
    AgentStarted { pid: u32 },
    #[serde(rename = "agent:stdout")]
    AgentStdout { data: String },
    #[serde(rename = "agent:stderr")]
    AgentStderr { data: String },
    #[serde(rename = "agent:exit")]
    AgentExit {
        code: Option<i32>,
        signal: Option<String>,
    },
    #[serde(rename = "exec:result")]
    ExecResult {
        id: String,
        code: i32,
        stdout: String,
        stderr: String,
    },
}

/// Write a protocol event to stdout (§4.3: single-line JSON, newline-delimited).
fn emit(ev: &Ev) {
    let json = serde_json::to_string(ev).expect("event serialization failed");
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = writeln!(out, "{json}");
    let _ = out.flush();
}

/// Log to stderr (supervisor logging must not go to stdout — §4.3).
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("[supervisor] {}", format!($($arg)*));
    };
}

// ---------------------------------------------------------------------------
// Repo provisioning (spec/session-runtime.md §3)
// ---------------------------------------------------------------------------

const WORK_DIR: &str = "/workspace";

/// Check if the workspace already has a git repo (workspace reuse).
fn repo_exists() -> bool {
    std::process::Command::new("git")
        .args(["-C", WORK_DIR, "rev-parse", "--git-dir"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Clone the repo and check out the branch. Embeds GITHUB_TOKEN in the URL
/// for HTTPS auth (spec §3.1).
fn clone_repo(url: &str, branch: &str) -> Result<(), String> {
    // Configure git identity.
    let _ = std::process::Command::new("git")
        .args(["config", "--global", "user.email", "tasks@localhost"])
        .status();
    let _ = std::process::Command::new("git")
        .args(["config", "--global", "user.name", "Tasks Agent"])
        .status();

    // Embed token in URL for auth.
    let clone_url = match std::env::var("GITHUB_TOKEN") {
        Ok(token) if url.starts_with("https://github.com/") => {
            url.replace("https://github.com/", &format!("https://x-access-token:{token}@github.com/"))
        }
        _ => url.to_string(),
    };

    let status = std::process::Command::new("git")
        .args(["clone", &clone_url, WORK_DIR])
        .stderr(Stdio::inherit()) // let clone progress go to container stderr
        .status()
        .map_err(|e| format!("git clone failed to start: {e}"))?;

    if !status.success() {
        return Err(format!("git clone exited with {status}"));
    }

    let status = std::process::Command::new("git")
        .args(["-C", WORK_DIR, "checkout", "-B", branch])
        .status()
        .map_err(|e| format!("git checkout failed to start: {e}"))?;

    if !status.success() {
        return Err(format!("git checkout exited with {status}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Agent management (spec §9.4, session-runtime.md §4.1)
// ---------------------------------------------------------------------------

const KILL_TIMEOUT_SECS: u64 = 5;

struct AgentHandle {
    child: Child,
}

/// Start the agent process. Returns the handle and spawns tasks that
/// forward stdout/stderr as protocol events via the provided channel.
async fn start_agent(
    prompt: &str,
    event_tx: &mpsc::UnboundedSender<Ev>,
) -> Result<AgentHandle, String> {
    let agent_cmd = std::env::var("AGENT_CMD").unwrap_or_else(|_| "claude".to_string());
    let agent_args: Vec<String> = std::env::var("AGENT_ARGS")
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_else(|_| vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ]);

    let mut cmd = Command::new(&agent_cmd);
    cmd.args(&agent_args)
        .arg(prompt)
        .current_dir(WORK_DIR)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn agent: {e}"))?;
    let pid = child.id().unwrap_or(0);

    // Spawn stdout reader.
    if let Some(stdout) = child.stdout.take() {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = tx.send(Ev::AgentStdout { data });
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // Spawn stderr reader.
    if let Some(stderr) = child.stderr.take() {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = tx.send(Ev::AgentStderr { data });
                    }
                    Err(_) => break,
                }
            }
        });
    }

    event_tx.send(Ev::AgentStarted { pid }).ok();
    Ok(AgentHandle { child })
}

/// Stop the agent: SIGTERM → timeout → SIGKILL (spec §4.1 stop command).
async fn stop_agent(handle: &mut AgentHandle) {
    // Send SIGTERM.
    if let Err(e) = handle.child.start_kill() {
        log!("failed to send kill signal: {e}");
        return;
    }

    // Wait with timeout.
    match tokio::time::timeout(
        std::time::Duration::from_secs(KILL_TIMEOUT_SECS),
        handle.child.wait(),
    )
    .await
    {
        Ok(_) => {} // exited
        Err(_) => {
            // Timeout — force kill.
            log!("agent did not exit in {KILL_TIMEOUT_SECS}s, sending SIGKILL");
            let _ = handle.child.kill().await;
        }
    }
}

/// Send a chat message to the agent's stdin (spec §4.1 chat command).
async fn send_chat(handle: &mut AgentHandle, text: &str) {
    if let Some(ref mut stdin) = handle.child.stdin {
        let msg = format!("{text}\n");
        let _ = stdin.write_all(msg.as_bytes()).await;
        let _ = stdin.flush().await;
    }
}

// ---------------------------------------------------------------------------
// Exec command (spec §4.1)
// ---------------------------------------------------------------------------

async fn exec_command(id: &str, argv: &[String]) -> Ev {
    if argv.is_empty() {
        return Ev::ExecResult {
            id: id.to_string(),
            code: 1,
            stdout: String::new(),
            stderr: "empty argv".to_string(),
        };
    }

    match Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(WORK_DIR)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(output) => Ev::ExecResult {
            id: id.to_string(),
            code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        },
        Err(e) => Ev::ExecResult {
            id: id.to_string(),
            code: 1,
            stdout: String::new(),
            stderr: format!("{e}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Event channel — agent stdout/stderr readers send events here,
    // the main loop emits them to the host.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Ev>();

    // Emit system:ready (§4.2).
    emit(&Ev::SystemReady {});

    // Read commands from stdin on a blocking thread (stdin is sync).
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Cmd>(&line) {
                Ok(cmd) => {
                    if cmd_tx.send(cmd).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    // Malformed command — ignore and log warning (§4.3).
                    log!("ignoring malformed command: {line}");
                }
            }
        }
    });

    // Set up SIGTERM handler for graceful shutdown (spec §4.1 stop).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    let mut agent: Option<AgentHandle> = None;
    let mut pending_chat: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            // Commands from host.
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    Cmd::Start { repo, branch, prompt } => {
                        // Clone repo if needed (§3).
                        if !repo_exists() {
                            if let Err(e) = clone_repo(&repo, &branch) {
                                log!("clone failed: {e}");
                                emit(&Ev::AgentExit { code: Some(1), signal: None });
                                continue;
                            }
                        }
                        // Start agent.
                        match start_agent(&prompt, &event_tx).await {
                            Ok(mut handle) => {
                                // Flush pending chat messages.
                                for text in pending_chat.drain(..) {
                                    send_chat(&mut handle, &text).await;
                                }
                                agent = Some(handle);
                            }
                            Err(e) => {
                                log!("agent start failed: {e}");
                                emit(&Ev::AgentExit { code: Some(1), signal: None });
                            }
                        }
                    }
                    Cmd::Chat { text } => {
                        if let Some(ref mut handle) = agent {
                            send_chat(handle, &text);
                        } else {
                            // Buffer if agent not running (§4.1).
                            pending_chat.push(text);
                        }
                    }
                    Cmd::Stop => {
                        if let Some(ref mut handle) = agent {
                            stop_agent(handle).await;
                        }
                    }
                    Cmd::Exec { id, argv } => {
                        let result = exec_command(&id, &argv).await;
                        emit(&result);
                    }
                }
            }
            // Events from agent stdout/stderr readers.
            Some(ev) = event_rx.recv() => {
                emit(&ev);
            }
            // Agent process exit.
            _ = async {
                if let Some(ref mut handle) = agent {
                    handle.child.wait().await.ok();
                } else {
                    // No agent — sleep forever so this branch doesn't fire.
                    std::future::pending::<()>().await;
                }
            } => {
                if let Some(mut handle) = agent.take() {
                    let status = handle.child.try_wait().ok().flatten();
                    let code = status.and_then(|s| s.code());
                    // Check for signal (Unix only).
                    #[cfg(unix)]
                    let signal = {
                        use std::os::unix::process::ExitStatusExt;
                        status.and_then(|s| s.signal()).map(|s| format!("{s}"))
                    };
                    #[cfg(not(unix))]
                    let signal: Option<String> = None;

                    emit(&Ev::AgentExit { code, signal });
                }
            }
            // SIGTERM — graceful shutdown.
            _ = sigterm.recv() => {
                if let Some(ref mut handle) = agent {
                    stop_agent(handle).await;
                }
                break;
            }
        }
    }
}
