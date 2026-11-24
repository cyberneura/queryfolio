//! Child processes of the coding agents (CYBERNEURA-DEV-1354).
//!
//! Two kinds of runs:
//!
//! - **Chat** (`agent_start` / `agent_write` / `agent_stop`): a long-lived agent (Claude Code
//!   stream-json, Codex app-server). The JSON lines on its stdin / stdout are relayed to the
//!   frontend without being interpreted here; the protocol lives in `src/lib/agentChat`.
//! - **One shot** (`agent_run_once` / `agent_cancel_once`): the inline AI field above the
//!   editor. One prompt, the final message is the result.
//!
//! The supervision of the chat processes (generation IDs, SIGTERM before SIGKILL, waiting for
//! the reaping threads on exit) is adapted from Beckwell (Cyberneura, MIT) via Astragal. See
//! docs/agent-chat.md.

use crate::agent::{self, AgentConfig, AgentDirectories};
use crate::error::AppError;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::sync::CancellationToken;

/// Grace period between asking a process to exit and killing it
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Longest one-shot run. An agent that edits a file usually finishes within a minute or two;
/// the limit only stops a run that hangs
const ONCE_TIMEOUT: Duration = Duration::from_secs(600);

/// How much of a failed one-shot run's stderr is returned in the error
const ONCE_MAX_STDERR_CHARS: usize = 2_000;

struct AgentProcess {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
}

#[derive(Default)]
struct Processes {
    map: HashMap<u32, AgentProcess>,
    /// The largest ID accepted so far. The frontend numbers from the next one after a page
    /// reload, so that the leftover events of an earlier process never reach a new conversation
    highest_id: u32,
    /// Advanced by every stop_all. The frontend passes the generation it got from agent_reset
    /// to agent_start, so that a start requested by the page before a reload is refused
    generation: u64,
    /// Threads waiting for stopped processes to exit. Joined when the app exits, so that the
    /// SIGKILL fallback has run by then
    reaping: Vec<JoinHandle<()>>,
}

#[derive(Default)]
pub struct AgentState {
    processes: Mutex<Processes>,
    /// Running one-shot runs by the ID the frontend gave them
    once: Mutex<HashMap<String, CancellationToken>>,
}

#[derive(Clone, Serialize)]
struct AgentLine {
    id: u32,
    line: String,
}

#[derive(Clone, Serialize)]
struct AgentExit {
    id: u32,
    code: Option<i32>,
}

#[derive(Clone, Serialize)]
pub struct AgentStarted {
    /// The directory the agent was started in
    cwd: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentReset {
    highest_id: u32,
    generation: u64,
}

/// Asks a process to exit. SIGKILL would not let Claude Code stop the command of a running
/// tool, which would then be left behind as an orphan; SIGTERM gives it the chance.
/// `kill(1)` is used instead of libc to avoid a direct dependency for one call.
fn send_sigterm(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => return None,
        }
    }
}

/// Collects a process that was sent SIGTERM; kills it when it does not exit in time.
/// Exiting the app waits for this, so every wait is bounded.
fn reap(mut child: Child) {
    if wait_exit(&mut child, EXIT_TIMEOUT).is_some() {
        return;
    }
    let _ = child.kill();
    if wait_exit(&mut child, EXIT_TIMEOUT).is_some() {
        return;
    }
    // Nothing more can be done for a process that survives SIGKILL. Do not keep the caller
    // waiting; only keep collecting it
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Ends a process that was removed from the list. SIGTERM is sent here; the wait happens on
/// another thread so that the caller is not held up
fn terminate(processes: &mut Processes, process: AgentProcess) {
    send_sigterm(process.child.id());
    processes.reaping.retain(|handle| !handle.is_finished());
    processes
        .reaping
        .push(std::thread::spawn(move || reap(process.child)));
}

/// Ends every remaining process and advances the generation
fn stop_all(processes: &mut Processes) {
    processes.generation += 1;
    let drained: Vec<AgentProcess> = processes.map.drain().map(|(_, p)| p).collect();
    for process in drained {
        terminate(processes, process);
    }
}

/// Called when the app exits. If the app exited while the reaping threads were still waiting,
/// the SIGKILL fallback would never run, so wait for them
pub fn stop_all_and_wait(app_handle: &AppHandle) {
    let state = app_handle.state::<AgentState>();
    let handles = {
        let mut processes = state.processes.lock().unwrap();
        stop_all(&mut processes);
        std::mem::take(&mut processes.reaping)
    };
    for token in state.once.lock().unwrap().values() {
        token.cancel();
    }
    for handle in handles {
        let _ = handle.join();
    }
}

/// Ends every chat agent (settings reload: the provider, the directories and the connections
/// may all have changed). The frontend drops its conversations at the same time.
pub fn stop_all_chats(app_handle: &AppHandle) {
    let state = app_handle.state::<AgentState>();
    let mut processes = state.processes.lock().unwrap();
    stop_all(&mut processes);
}

/// Reads stdout / stderr line by line. One line of invalid UTF-8 must not stop the relay, so
/// each line is converted lossily
fn for_each_line(reader: impl std::io::Read, mut on_line: impl FnMut(String)) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        if !line.trim().is_empty() {
            on_line(line.to_string());
        }
    }
}

/// The user's login shell, used to start the agent with the PATH of their terminal (a GUI
/// launch inherits a minimal PATH; `claude` and `codex` live in npm / nvm / ~/.local
/// directories that only the shell profile adds)
#[cfg(unix)]
fn login_shell() -> Option<std::path::PathBuf> {
    let shell = std::env::var_os("SHELL")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .or_else(|| {
            ["/bin/zsh", "/bin/bash"]
                .into_iter()
                .map(std::path::PathBuf::from)
                .find(|path| path.is_file())
        })?;
    matches!(
        shell.file_name().and_then(|s| s.to_str()),
        Some("zsh" | "bash" | "ksh" | "sh")
    )
    .then_some(shell)
}

/// Builds the command that runs `argv` in `cwd`.
///
/// On Unix it goes through the login shell (interactive when the shell accepts it, so that
/// PATH additions in .zshrc count too). The agent's stdin / stdout / stderr are kept on other
/// descriptors while the profile runs, so that nothing a profile prints reaches the JSON pipes,
/// and the arguments are passed as positional parameters (`"$@"`), never as shell text — the
/// prompt of a one-shot run is user input.
fn agent_command(cwd: &Path, argv: &[String]) -> Command {
    #[cfg(unix)]
    if let Some(shell) = login_shell() {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                concat!(
                    "exec 3>&1 4>&2 5<&0 1>/dev/null 2>/dev/null </dev/null; ",
                    "s=$1; shift; ",
                    "if \"$s\" -lic ': >&3' </dev/null; then f=-lic; else f=-lc; fi; ",
                    "exec \"$s\" \"$f\" 'cd -- \"$1\" 2>&4 || exit 1; shift; ",
                    "exec \"$@\" 0<&5 1>&3 2>&4 3>&- 4>&- 5<&-' queryfolio \"$@\""
                ),
                "queryfolio",
            ])
            .arg(shell)
            .arg(cwd)
            .args(argv)
            .current_dir(cwd);
        return command;
    }
    let mut command = Command::new(resolve_program(&argv[0]));
    command.args(&argv[1..]).current_dir(cwd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: do not flash a console window
        command.creation_flags(0x0800_0000);
    }
    command
}

/// Finds the program on PATH. On Windows the CLIs installed by npm are `.cmd` shims, which
/// `Command::new("claude")` does not find (it only tries `.exe`); the standard library escapes
/// the arguments of a `.cmd` / `.bat` itself and refuses the ones it cannot escape.
fn resolve_program(name: &str) -> std::path::PathBuf {
    #[cfg(windows)]
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for ext in ["exe", "cmd", "bat"] {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    std::path::PathBuf::from(name)
}

/// Called when the frontend initializes. Ends the processes a previous page left behind; the
/// frontend numbers from `highest_id + 1` and passes the generation to agent_start
#[tauri::command]
pub async fn agent_reset(app_handle: AppHandle) -> Result<AgentReset, AppError> {
    let state = app_handle.state::<AgentState>();
    let mut processes = state.processes.lock().unwrap();
    stop_all(&mut processes);
    Ok(AgentReset {
        highest_id: processes.highest_id,
        generation: processes.generation,
    })
}

/// Starts the chat agent of a connection. `directory` is a working directory chosen in the
/// chat pane (None: the configured one, then the connection's query file folder).
#[tauri::command]
pub async fn agent_start(
    id: u32,
    generation: u64,
    connection: String,
    directory: Option<String>,
    app_handle: AppHandle,
    state: tauri::State<'_, super::AppState>,
) -> Result<AgentStarted, AppError> {
    let (config, dirs) = state
        .resolve_agent_launch(&connection, directory.as_deref())
        .await?;
    let argv = agent::chat_argv(&config, &dirs);
    let agents = app_handle.state::<AgentState>();
    // Hold the lock from the generation check to the registration: a stop_all (page
    // initialization, app exit) between the spawn and the registration could not stop a
    // process that is not in the list yet
    let (stdout, stderr) = {
        let mut processes = agents.processes.lock().unwrap();
        if generation != processes.generation {
            return Err(AppError::Agent("The agent chats were reset".into()));
        }
        if processes.map.contains_key(&id) {
            return Err(AppError::Agent(format!("Agent process {id} already exists")));
        }
        processes.highest_id = processes.highest_id.max(id);

        let mut child = agent_command(&dirs.cwd, &argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| start_error(&config, &e))?;

        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            send_sigterm(child.id());
            reap(child);
            return Err(AppError::Agent("Failed to open the pipes of the agent".into()));
        };
        processes.map.insert(
            id,
            AgentProcess {
                child,
                stdin: Arc::new(Mutex::new(stdin)),
            },
        );
        (stdout, stderr)
    };

    let app_for_stderr = app_handle.clone();
    std::thread::spawn(move || {
        for_each_line(stderr, |line| {
            let _ = app_for_stderr.emit("agent-stderr", AgentLine { id, line });
        });
    });

    let app_for_stdout = app_handle.clone();
    std::thread::spawn(move || {
        for_each_line(stdout, |line| {
            let _ = app_for_stdout.emit("agent-event", AgentLine { id, line });
        });

        // The process can outlive its stdout (or a read error). Keep it in the list, so that
        // agent_stop / stop_all can still stop it, until its exit is confirmed; end it from
        // here when the grace period passes
        let state = app_for_stdout.state::<AgentState>();
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let code = loop {
            let mut processes = state.processes.lock().unwrap();
            // Not in the list: agent_stop / stop_all removed it and handles its exit (no
            // agent-exit event is sent either)
            let Some(process) = processes.map.get_mut(&id) else {
                return;
            };
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    processes.map.remove(&id);
                    break status.code();
                }
                Ok(None) if Instant::now() < deadline => {}
                _ => {
                    if let Some(process) = processes.map.remove(&id) {
                        terminate(&mut processes, process);
                    }
                    break None;
                }
            }
            drop(processes);
            std::thread::sleep(Duration::from_millis(50));
        };
        let _ = app_for_stdout.emit("agent-exit", AgentExit { id, code });
    });

    Ok(AgentStarted {
        cwd: dirs.cwd.to_string_lossy().into_owned(),
    })
}

fn start_error(config: &AgentConfig, error: &std::io::Error) -> AppError {
    AppError::Agent(format!(
        "Failed to start {}: {error}. Install it and sign in, then try again",
        config.provider.display_name()
    ))
}

/// Writes one JSON line to the agent's stdin (the frontend builds the JSON)
#[tauri::command]
pub async fn agent_write(id: u32, line: String, app_handle: AppHandle) -> Result<(), AppError> {
    let stdin = {
        let state = app_handle.state::<AgentState>();
        let processes = state.processes.lock().unwrap();
        processes
            .map
            .get(&id)
            .map(|p| Arc::clone(&p.stdin))
            .ok_or_else(|| AppError::Agent(format!("Agent process {id} not found")))?
    };
    // A pipe write can block while the agent is busy; keep it off the async runtime
    tokio::task::spawn_blocking(move || {
        let mut stdin = stdin.lock().unwrap();
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|e| AppError::Agent(format!("Failed to write to the agent: {e}")))
    })
    .await
    .map_err(|e| AppError::Agent(format!("Failed to write to the agent: {e}")))?
}

/// Stops an agent. No agent-exit event is sent for a process stopped here: the side that
/// stopped it (the frontend) updates its own state
#[tauri::command]
pub async fn agent_stop(id: u32, app_handle: AppHandle) -> Result<(), AppError> {
    let state = app_handle.state::<AgentState>();
    let mut processes = state.processes.lock().unwrap();
    if let Some(process) = processes.map.remove(&id) {
        terminate(&mut processes, process);
    }
    Ok(())
}

/// Result of a one-shot run
#[derive(Debug, Serialize)]
pub struct OnceResult {
    /// The agent's final message (clipped to agent::ONCE_MAX_OUTPUT_CHARS)
    pub output: String,
    /// The directory the agent ran in
    pub cwd: String,
}

/// Runs the agent once with `prompt` in the connection's agent directory and returns its final
/// message. The agent may edit files there (and in the query file folder); it cannot run
/// commands (see agent::once_argv). `run_id` is chosen by the frontend for agent_cancel_once.
#[tauri::command]
pub async fn agent_run_once(
    run_id: String,
    connection: String,
    prompt: String,
    app_handle: AppHandle,
    state: tauri::State<'_, super::AppState>,
) -> Result<OnceResult, AppError> {
    if prompt.trim().is_empty() {
        return Err(AppError::Agent("The instruction is empty".into()));
    }
    let (config, dirs) = state.resolve_agent_launch(&connection, None).await?;
    let token = CancellationToken::new();
    {
        let agents = app_handle.state::<AgentState>();
        let mut once = agents.once.lock().unwrap();
        if once.contains_key(&run_id) {
            return Err(AppError::Agent("This agent run already exists".into()));
        }
        once.insert(run_id.clone(), token.clone());
    }
    let result = run_once(&config, &dirs, &prompt, &token).await;
    app_handle
        .state::<AgentState>()
        .once
        .lock()
        .unwrap()
        .remove(&run_id);
    result
}

/// Cancels a one-shot run (the run then fails with AppError::Cancelled)
#[tauri::command]
pub async fn agent_cancel_once(run_id: String, app_handle: AppHandle) -> Result<(), AppError> {
    if let Some(token) = app_handle
        .state::<AgentState>()
        .once
        .lock()
        .unwrap()
        .get(&run_id)
    {
        token.cancel();
    }
    Ok(())
}

async fn run_once(
    config: &AgentConfig,
    dirs: &AgentDirectories,
    prompt: &str,
    token: &CancellationToken,
) -> Result<OnceResult, AppError> {
    // Codex writes its final message here. A private directory: the message can quote the
    // query file
    let scratch = ScratchDir::create()?;
    let last_message_file = scratch.path.join("last-message.txt");
    let argv = agent::once_argv(config, dirs, prompt, &last_message_file);

    let mut command = tokio::process::Command::from(agent_command(&dirs.cwd, &argv));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|e| start_error(config, &e))?;
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // Drain both pipes while waiting: a full pipe would block the agent forever
    let read_stdout = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut buf).await;
        buf
    });
    let read_stderr = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await;
        buf
    });

    let status = tokio::select! {
        status = child.wait() => Some(status?),
        _ = token.cancelled() => None,
        _ = tokio::time::sleep(ONCE_TIMEOUT) => None,
    };
    let Some(status) = status else {
        // Cancelled or timed out: SIGTERM first (the agent stops its own children), then kill
        if let Some(pid) = pid {
            send_sigterm(pid);
        }
        if tokio::time::timeout(EXIT_TIMEOUT, child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
        return Err(if token.is_cancelled() {
            AppError::Cancelled
        } else {
            AppError::Agent(format!(
                "{} did not finish within {} minutes and was stopped",
                config.provider.display_name(),
                ONCE_TIMEOUT.as_secs() / 60
            ))
        });
    };
    let stdout = String::from_utf8_lossy(&read_stdout.await.unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&read_stderr.await.unwrap_or_default()).into_owned();
    if !status.success() {
        let detail = tail_chars(
            if stderr.trim().is_empty() { &stdout } else { &stderr },
            ONCE_MAX_STDERR_CHARS,
        );
        return Err(AppError::Agent(format!(
            "{} failed ({status}): {detail}",
            config.provider.display_name()
        )));
    }
    let last_message = std::fs::read_to_string(&last_message_file).unwrap_or_default();
    let output = if last_message.trim().is_empty() {
        stdout
    } else {
        last_message
    };
    Ok(OnceResult {
        output: agent::clip_output(&output),
        cwd: dirs.cwd.to_string_lossy().into_owned(),
    })
}

/// A private (0700) directory under the temp directory, removed when dropped
struct ScratchDir {
    path: std::path::PathBuf,
}

impl ScratchDir {
    fn create() -> Result<Self, AppError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "queryfolio-agent-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        // Not recursive: an existing path (planted by someone else) is an error, not reused
        builder.create(&path)?;
        Ok(ScratchDir { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The last `max` characters of `text` (the end of stderr is where the error is)
fn tail_chars(text: &str, max: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut tail: String = text.chars().skip(count - max).collect();
    tail.insert(0, '…');
    tail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_end_of_long_output_is_kept() {
        assert_eq!(tail_chars(" short \n", 10), "short");
        assert_eq!(tail_chars("0123456789", 4), "…6789");
    }

    #[test]
    fn the_scratch_directory_is_private_and_removed() {
        let scratch = ScratchDir::create().unwrap();
        let path = scratch.path.clone();
        assert!(path.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        std::fs::write(path.join("last-message.txt"), "x").unwrap();
        drop(scratch);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn arguments_reach_the_program_unchanged_through_the_login_shell() {
        // The prompt of a one-shot run is user text: quotes, `$`, backticks and newlines must
        // arrive as one argument and never be evaluated by the shell
        let tmp = tempfile::tempdir().unwrap();
        let tricky = "it's \"x\" $(touch pwned) `touch pwned2` ; rm -rf ~\nline 2";
        let argv = vec![
            "printf".to_string(),
            "%s|".to_string(),
            "--".to_string(),
            tricky.to_string(),
        ];
        let output = agent_command(tmp.path(), &argv)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("--|{tricky}|")
        );
        assert!(!tmp.path().join("pwned").exists());
        assert!(!tmp.path().join("pwned2").exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_program_runs_in_the_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let output = agent_command(tmp.path(), &["pwd".to_string(), "-P".to_string()])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            tmp.path().canonicalize().unwrap().to_string_lossy()
        );
    }
}
