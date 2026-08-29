//! Runs `claude auth login --claudeai` inside an isolated `CLAUDE_CONFIG_DIR`,
//! with cancellation, timeouts, and combined output capture. Structurally a
//! port of `codex_accounts::login_runner` (same cancellation shape), but
//! spawns `claude auth login --claudeai` with `CLAUDE_CONFIG_DIR` instead of
//! `codex login` with `CODEX_HOME`.
//!
//! Threat Matrix — Subprocess env scoping (Applicable): `args` is always the
//! fixed `LOGIN_ARGS` slice, never shell-composed from user text; the spawned
//! process's only *overridden* env var is `CLAUDE_CONFIG_DIR`, and `dir` is
//! always an app-generated path under `managed-configs/<uuid>`, never raw user
//! input.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fixed argv for the login subprocess. Never shell-composed with user input.
const LOGIN_ARGS: &[&str] = &["auth", "login", "--claudeai"];

/// Outcome of a `claude auth login` subprocess run.
#[derive(Debug, Clone)]
pub enum ClaudeLoginOutcome {
    MissingBinary,
    LaunchFailed(String),
    TimedOut(String),
    Cancelled,
    Failed(String),
    Success(String),
}

impl ClaudeLoginOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClaudeLoginOutcome::MissingBinary => "missing_binary",
            ClaudeLoginOutcome::LaunchFailed(_) => "launch_failed",
            ClaudeLoginOutcome::TimedOut(_) => "timed_out",
            ClaudeLoginOutcome::Cancelled => "cancelled",
            ClaudeLoginOutcome::Failed(_) => "failed",
            ClaudeLoginOutcome::Success(_) => "success",
        }
    }

    pub fn output(&self) -> &str {
        match self {
            ClaudeLoginOutcome::MissingBinary => "",
            ClaudeLoginOutcome::LaunchFailed(output)
            | ClaudeLoginOutcome::TimedOut(output)
            | ClaudeLoginOutcome::Failed(output)
            | ClaudeLoginOutcome::Success(output) => output,
            ClaudeLoginOutcome::Cancelled => "",
        }
    }
}

/// Result of a `claude auth login` subprocess run.
#[derive(Debug, Clone)]
pub struct ClaudeLoginResult {
    pub outcome: ClaudeLoginOutcome,
}

/// Handle around an in-flight `claude auth login` process, for cancellation.
#[derive(Debug, Default, Clone)]
pub struct ManagedLoginProcess {
    inner: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl ManagedLoginProcess {
    fn bind(&self, process: Child) {
        *self.inner.lock().expect("login process lock") = Some(process);
        self.cancelled.store(false, Ordering::SeqCst);
    }

    fn clear(&self) {
        *self.inner.lock().expect("login process lock") = None;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let mut guard = self.inner.lock().expect("login process lock");
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
        }
    }
}

thread_local! {
    /// Test-only seam for the RED tests below: `None` = use the real
    /// `which::which("claude")` lookup; `Some(None)` = force `MissingBinary`;
    /// `Some(Some(path))` = force a specific resolved binary path.
    static BINARY_OVERRIDE: RefCell<Option<Option<PathBuf>>> = const { RefCell::new(None) };
}

/// Force `locate_claude_binary` to return `value` for this thread (tests
/// only). Pass `None` to simulate a missing `claude` binary.
#[cfg(test)]
pub(crate) fn with_claude_binary_override(value: Option<PathBuf>) {
    BINARY_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(value));
}

#[cfg(test)]
pub(crate) fn clear_claude_binary_override() {
    BINARY_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
}

/// Runs `claude auth login --claudeai` inside an isolated `CLAUDE_CONFIG_DIR`.
pub struct ClaudeLoginRunner;

impl ClaudeLoginRunner {
    /// Resolve the `claude` executable via `PATH`.
    pub fn locate_claude_binary() -> Option<PathBuf> {
        if let Some(overridden) = BINARY_OVERRIDE.with(|cell| cell.borrow().clone()) {
            return overridden;
        }
        which::which("claude").ok()
    }

    pub fn run(
        dir: &Path,
        timeout: Duration,
        handle: Option<&ManagedLoginProcess>,
    ) -> ClaudeLoginResult {
        let active_handle = handle.cloned().unwrap_or_default();
        let Some(binary) = Self::locate_claude_binary() else {
            return ClaudeLoginResult {
                outcome: ClaudeLoginOutcome::MissingBinary,
            };
        };

        let mut command = build_login_command(&binary, dir);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ClaudeLoginResult {
                    outcome: ClaudeLoginOutcome::LaunchFailed(error.to_string()),
                };
            }
        };
        active_handle.bind(child);

        let output = match wait_for_child(&active_handle, timeout) {
            Some(output) => output,
            None => {
                let output = kill_and_drain(&active_handle);
                active_handle.clear();
                return ClaudeLoginResult {
                    outcome: ClaudeLoginOutcome::TimedOut(combine_output(&output)),
                };
            }
        };

        active_handle.clear();
        let combined = combine_output(&output);
        if active_handle.is_cancelled() {
            return ClaudeLoginResult {
                outcome: ClaudeLoginOutcome::Cancelled,
            };
        }
        if output.status.success() {
            return ClaudeLoginResult {
                outcome: ClaudeLoginOutcome::Success(combined),
            };
        }
        ClaudeLoginResult {
            outcome: ClaudeLoginOutcome::Failed(combined),
        }
    }
}

/// Build the login subprocess command: fixed args, `Stdio::piped`, and
/// exactly one overridden env var (`CLAUDE_CONFIG_DIR`) on top of the
/// inherited environment.
fn build_login_command(binary: &Path, dir: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .args(LOGIN_ARGS)
        .env("CLAUDE_CONFIG_DIR", dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn wait_for_child(handle: &ManagedLoginProcess, timeout: Duration) -> Option<std::process::Output> {
    let deadline = Instant::now() + timeout;
    loop {
        if handle.is_cancelled() {
            let output = take_child(handle)?.wait_with_output().ok();
            return output;
        }
        let polled = {
            let mut guard = handle.inner.lock().expect("login process lock");
            match guard.as_mut().map(|child| child.try_wait()) {
                Some(Ok(Some(_status))) => take_child(handle)?.wait_with_output().ok(),
                Some(Err(_)) => take_child(handle)?.wait_with_output().ok(),
                _ => None,
            }
        };
        if polled.is_some() {
            return polled;
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn take_child(handle: &ManagedLoginProcess) -> Option<Child> {
    handle.inner.lock().expect("login process lock").take()
}

fn kill_and_drain(handle: &ManagedLoginProcess) -> std::process::Output {
    let mut child = take_child(handle).expect("login process present");
    let _ = child.kill();
    child
        .wait_with_output()
        .unwrap_or_else(|_| std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
}

fn combine_output(output: &std::process::Output) -> String {
    let mut parts: Vec<String> = Vec::new();
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    let merged = parts.join("\n");
    let merged = merged.trim();
    if merged.is_empty() {
        "No output captured.".to_string()
    } else {
        merged.chars().take(4000).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Threat Matrix: Subprocess env scoping — RED (task 1.13/1.14) ───────

    #[test]
    fn run_returns_missing_binary_when_claude_not_found() {
        with_claude_binary_override(None);
        let dir = tempfile::tempdir().unwrap();
        let result = ClaudeLoginRunner::run(dir.path(), Duration::from_secs(1), None);
        assert!(matches!(result.outcome, ClaudeLoginOutcome::MissingBinary));
        clear_claude_binary_override();
    }

    #[test]
    fn build_login_command_uses_fixed_args_and_scopes_only_config_dir_env() {
        let dir = PathBuf::from("/managed-configs/11111111-1111-1111-1111-111111111111");
        let command = build_login_command(Path::new("/usr/bin/claude"), &dir);

        let args: Vec<&str> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args, LOGIN_ARGS,
            "args must be the fixed slice, never shell-composed"
        );

        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(
            envs.len(),
            1,
            "exactly one overridden env var (CLAUDE_CONFIG_DIR), no ambient leakage"
        );
        assert_eq!(envs[0].0, "CLAUDE_CONFIG_DIR");
        assert_eq!(envs[0].1, Some(dir.as_os_str()));
    }
}
