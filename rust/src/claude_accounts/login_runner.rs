//! Runs `claude auth login --claudeai` inside an isolated `CLAUDE_CONFIG_DIR`,
//! with cancellation and timeouts. Structurally a port of
//! `codex_accounts::login_runner` (same cancellation shape), hardened with the
//! upstream saved-accounts login isolation (#22):
//!
//! - the child never inherits an auth override (`ANTHROPIC_API_KEY`,
//!   `CLAUDE_CODE_OAUTH_TOKEN`, Bedrock/Vertex/Foundry switches, ...), so the
//!   login always uses browser-based subscription auth;
//! - stdin/stdout/stderr are null: raw CLI output is never captured, so it can
//!   never be echoed into a user-facing error;
//! - on Windows the child joins a kill-on-close job object at creation, so the
//!   whole login process tree dies on cancel, timeout, or app exit.
//!
//! Threat Matrix — Subprocess env scoping (Applicable): `args` is always the
//! fixed `LOGIN_ARGS` slice, never shell-composed from user text; the only
//! *set* env var is `CLAUDE_CONFIG_DIR`, and `dir` is always an app-generated
//! path under `managed-configs/<uuid>`, never raw user input.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
mod windows_child;
#[cfg(windows)]
use windows_child::LoginChild;
#[cfg(not(windows))]
type LoginChild = std::process::Child;

/// Fixed argv for the login subprocess. Never shell-composed with user input.
const LOGIN_ARGS: &[&str] = &["auth", "login", "--claudeai"];

/// Environment variables that make Claude Code bypass subscription (OAuth)
/// login. Removed from every account subprocess; when set in CodexBar's own
/// environment they block switching (see [`require_cli_closed`]).
pub const AUTH_OVERRIDES: [&str; 7] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Set by Claude Code inside its own sessions; a nested login must not think
/// it runs inside one.
const NESTED_SESSION_MARKER: &str = "CLAUDECODE";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Outcome of a `claude auth login` subprocess run.
///
/// Deliberately carries no raw CLI output: only safe categories, a process
/// exit code, and a redacted launch diagnostic ever leave this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeLoginOutcome {
    MissingBinary,
    /// Spawning (or waiting on) the child failed; the payload is a redacted,
    /// length-capped diagnostic (see [`redact_diagnostic`]).
    LaunchFailed(String),
    TimedOut,
    Cancelled,
    /// The CLI exited unsuccessfully (exit code when the OS reports one).
    Failed(Option<i32>),
    Success,
}

impl ClaudeLoginOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClaudeLoginOutcome::MissingBinary => "missing_binary",
            ClaudeLoginOutcome::LaunchFailed(_) => "launch_failed",
            ClaudeLoginOutcome::TimedOut => "timed_out",
            ClaudeLoginOutcome::Cancelled => "cancelled",
            ClaudeLoginOutcome::Failed(_) => "failed",
            ClaudeLoginOutcome::Success => "success",
        }
    }
}

/// Result of a `claude auth login` subprocess run.
#[derive(Debug, Clone)]
pub struct ClaudeLoginResult {
    pub outcome: ClaudeLoginOutcome,
}

/// Handle around an in-flight `claude auth login` process, for cancellation.
///
/// A cancel requested before the child is bound is remembered: the child is
/// killed as soon as it is bound, so an early Cancel click is never lost.
#[derive(Default, Clone)]
pub struct ManagedLoginProcess {
    inner: Arc<Mutex<Option<LoginChild>>>,
    cancelled: Arc<AtomicBool>,
}

impl std::fmt::Debug for ManagedLoginProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedLoginProcess")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl ManagedLoginProcess {
    fn bind(&self, mut process: LoginChild) {
        let mut guard = self.inner.lock().expect("login process lock");
        if self.is_cancelled() {
            let _killed = process.kill();
        }
        *guard = Some(process);
    }

    fn take(&self) -> Option<LoginChild> {
        self.inner.lock().expect("login process lock").take()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let mut guard = self.inner.lock().expect("login process lock");
        if let Some(child) = guard.as_mut() {
            let _killed = child.kill();
        }
    }
}

thread_local! {
    /// Test-only seam: `None` = use the real `which::which("claude")` lookup;
    /// `Some(None)` = force `MissingBinary`; `Some(Some(path))` = force a
    /// specific resolved binary path.
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
        let Some(binary) = Self::locate_claude_binary() else {
            return ClaudeLoginResult {
                outcome: ClaudeLoginOutcome::MissingBinary,
            };
        };
        let mut command = build_login_command(&binary, dir);
        run_login_command(&mut command, timeout, handle)
    }
}

fn run_login_command(
    command: &mut Command,
    timeout: Duration,
    handle: Option<&ManagedLoginProcess>,
) -> ClaudeLoginResult {
    let active_handle = handle.cloned().unwrap_or_default();
    let child = match spawn_login_process(command) {
        Ok(child) => child,
        Err(error) => {
            return ClaudeLoginResult {
                outcome: ClaudeLoginOutcome::LaunchFailed(redact_diagnostic(&error.to_string())),
            };
        }
    };
    active_handle.bind(child);

    let outcome = match wait_for_child(&active_handle, timeout) {
        WaitResult::Exited(_) if active_handle.is_cancelled() => ClaudeLoginOutcome::Cancelled,
        WaitResult::Exited(status) if status.success() => ClaudeLoginOutcome::Success,
        WaitResult::Exited(status) => ClaudeLoginOutcome::Failed(status.code()),
        WaitResult::Cancelled => ClaudeLoginOutcome::Cancelled,
        WaitResult::TimedOut => ClaudeLoginOutcome::TimedOut,
        WaitResult::WaitFailed(error) => {
            ClaudeLoginOutcome::LaunchFailed(redact_diagnostic(&error.to_string()))
        }
    };
    ClaudeLoginResult { outcome }
}

fn spawn_login_process(command: &mut Command) -> io::Result<LoginChild> {
    #[cfg(windows)]
    {
        LoginChild::spawn(command)
    }
    #[cfg(not(windows))]
    {
        command.spawn()
    }
}

/// Build the login subprocess command: fixed args, null stdio, the isolated
/// `CLAUDE_CONFIG_DIR` as both env var and working directory, and every
/// auth-override env var removed from the inherited environment.
fn build_login_command(binary: &Path, dir: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .args(LOGIN_ARGS)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    isolate_account_subprocess_env(&mut command, dir);
    command
}

/// Scope a Claude Code subprocess to `dir` and strip every inherited auth
/// override, so it only ever sees that directory's subscription login.
pub(crate) fn isolate_account_subprocess_env(command: &mut Command, dir: &Path) {
    command.env("CLAUDE_CONFIG_DIR", dir);
    for key in AUTH_OVERRIDES {
        command.env_remove(key);
    }
    command.env_remove(NESTED_SESSION_MARKER);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

enum WaitResult {
    Exited(ExitStatus),
    Cancelled,
    TimedOut,
    WaitFailed(io::Error),
}

fn wait_for_child(handle: &ManagedLoginProcess, timeout: Duration) -> WaitResult {
    let deadline = Instant::now() + timeout;
    loop {
        let polled = {
            let mut guard = handle.inner.lock().expect("login process lock");
            match guard.as_mut() {
                Some(child) => child.try_wait(),
                None => return WaitResult::Cancelled,
            }
        };
        match polled {
            Ok(Some(status)) => {
                let _reaped = handle.take();
                return WaitResult::Exited(status);
            }
            Ok(None) => {}
            Err(error) => {
                kill_and_reap(handle);
                return WaitResult::WaitFailed(error);
            }
        }
        if handle.is_cancelled() {
            kill_and_reap(handle);
            return WaitResult::Cancelled;
        }
        if Instant::now() >= deadline {
            kill_and_reap(handle);
            return WaitResult::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn kill_and_reap(handle: &ManagedLoginProcess) {
    if let Some(mut child) = handle.take() {
        let _killed = child.kill();
        let _reaped = child.wait();
    }
}

/// Maximum length of any diagnostic that may reach a user-facing error.
pub const MAX_DIAGNOSTIC_CHARS: usize = 240;

/// Redact token-like material from a diagnostic string and cap its length.
///
/// Defense in depth for the one remaining free-text path (a spawn/wait OS
/// error): terminal escapes and control characters are stripped, anything
/// that looks like a credential (`Bearer`/`Basic` values, JSON/`key=value`
/// secret fields, Anthropic `sk-ant-` keys, long opaque letter+digit runs such
/// as JWTs) becomes `[redacted]`, and the result is bounded to
/// [`MAX_DIAGNOSTIC_CHARS`].
pub fn redact_diagnostic(text: &str) -> String {
    use crate::providers::claude::claude_swap::sanitize_display;
    use regex_lite::{Captures, Regex};
    use std::sync::LazyLock;

    static AUTH_HEADER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]+").expect("valid pattern")
    });
    static SECRET_FIELD: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)("?(?:access_?token|refresh_?token|id_?token|api_?key|authorization|client_?secret|password|secret|token)"?\s*[:=]\s*)"?[^\s",}]+"?"#,
        )
        .expect("valid pattern")
    });
    static ANTHROPIC_KEY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"sk-ant-[A-Za-z0-9_-]*").expect("valid pattern"));
    static OPAQUE_RUN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[A-Za-z0-9_\-.+=]{24,}").expect("valid pattern"));

    let cleaned = sanitize_display(text, usize::MAX);
    let redacted = AUTH_HEADER.replace_all(&cleaned, "${1} [redacted]");
    let redacted = SECRET_FIELD.replace_all(&redacted, "${1}[redacted]");
    let redacted = ANTHROPIC_KEY.replace_all(&redacted, "[redacted]");
    let redacted = OPAQUE_RUN.replace_all(&redacted, |caps: &Captures<'_>| {
        let run = &caps[0];
        // Long plain words stay readable; letter+digit runs are token-like.
        if run.chars().any(|c| c.is_ascii_alphabetic()) && run.chars().any(|c| c.is_ascii_digit()) {
            "[redacted]".to_string()
        } else {
            run.to_string()
        }
    });

    let trimmed = redacted.trim();
    if trimmed.chars().count() <= MAX_DIAGNOSTIC_CHARS {
        return trimmed.to_string();
    }
    let mut capped: String = trimmed.chars().take(MAX_DIAGNOSTIC_CHARS).collect();
    capped.push('…');
    capped
}

// ── Switch guard: refuse while Claude Code CLI sessions are running ──────

// Electron Desktop also uses claude.exe. Its Windows version resource says
// "Claude"; the native CLI's says "Claude Code". Keep Desktop running.
#[cfg(windows)]
const CLI_PROCESS_COUNT_SCRIPT: &str = r"@(Get-Process | Where-Object { ($env:CODEXBAR_CLAUDE_EXE -and $_.Path -eq $env:CODEXBAR_CLAUDE_EXE) -or $_.Path -like '*\.local\share\claude\versions\*' -or ($_.ProcessName -eq 'claude' -and $_.MainModule.FileVersionInfo.ProductName -ne 'Claude') }).Count";

/// User-facing error when a Claude Code CLI session is still running.
pub const CLI_RUNNING_MESSAGE: &str =
    "Close your running Claude Code CLI sessions, then switch accounts and reopen Claude Code.";

/// Refuse when an inherited auth override would make Claude Code ignore the
/// switched subscription login.
fn require_subscription_environment(get: impl Fn(&str) -> Option<OsString>) -> io::Result<()> {
    for key in AUTH_OVERRIDES {
        if get(key).is_some_and(|value| !value.is_empty()) {
            return Err(io::Error::other(format!(
                "{key} overrides Claude subscription login. Unset it and restart CodexBar before switching accounts."
            )));
        }
    }
    Ok(())
}

/// Existing CLI processes keep credentials in memory and may rotate them
/// back over a switched login. Require them to exit first; account
/// management never terminates user tasks. The process scan is Windows-only
/// (mirrors upstream); every platform gets the auth-override check.
pub fn require_cli_closed() -> io::Result<()> {
    require_subscription_environment(|key| std::env::var_os(key))?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let exe = ClaudeLoginRunner::locate_claude_binary()
            .and_then(|path| path.canonicalize().ok())
            .map(|path| {
                path.to_string_lossy()
                    .trim_start_matches(r"\\?\")
                    .to_string()
            })
            .unwrap_or_default();
        let output = Command::new("powershell.exe")
            .env("CODEXBAR_CLAUDE_EXE", exe)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                CLI_PROCESS_COUNT_SCRIPT,
            ])
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                "Could not check whether Claude Code is running.",
            ));
        }
        let count: usize = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| io::Error::other("Could not check whether Claude Code is running."))?;
        if count > 0 {
            return Err(io::Error::other(CLI_RUNNING_MESSAGE));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Threat Matrix: Subprocess env scoping ───────────────────────────────

    #[test]
    fn run_returns_missing_binary_when_claude_not_found() {
        with_claude_binary_override(None);
        let dir = tempfile::tempdir().unwrap();
        let result = ClaudeLoginRunner::run(dir.path(), Duration::from_secs(1), None);
        assert_eq!(result.outcome, ClaudeLoginOutcome::MissingBinary);
        clear_claude_binary_override();
    }

    #[test]
    fn build_login_command_uses_fixed_args_and_isolates_env() {
        let dir = PathBuf::from("/managed-configs/11111111-1111-1111-1111-111111111111");
        let command = build_login_command(Path::new("/usr/bin/claude"), &dir);

        let args: Vec<&str> = command.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args, LOGIN_ARGS,
            "args must be the fixed slice, never shell-composed"
        );
        assert_eq!(command.get_current_dir(), Some(dir.as_path()));

        let envs: Vec<_> = command.get_envs().collect();
        let set: Vec<_> = envs.iter().filter(|(_, value)| value.is_some()).collect();
        assert_eq!(set.len(), 1, "only CLAUDE_CONFIG_DIR is ever set");
        assert_eq!(set[0].0, "CLAUDE_CONFIG_DIR");
        assert_eq!(set[0].1, Some(dir.as_os_str()));

        for key in AUTH_OVERRIDES.iter().chain(&[NESTED_SESSION_MARKER]) {
            assert!(
                envs.iter().any(|(k, v)| k == key && v.is_none()),
                "{key} must be removed from the login child"
            );
        }
    }

    #[test]
    fn auth_override_in_app_environment_blocks_switching() {
        let descriptor = "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR";
        let error = require_subscription_environment(|key| {
            (key == descriptor).then(|| OsString::from("3"))
        })
        .unwrap_err();
        assert!(error.to_string().contains(descriptor));
        assert!(require_subscription_environment(|_| None).is_ok());
        assert!(require_subscription_environment(|_| Some(OsString::new())).is_ok());
    }

    #[test]
    fn redaction_strips_credentials_and_caps_length() {
        let secrets = [
            "sk-ant-oat01-AbCdEf0123456789",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
            "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
            "rt-supersecret",
            "hunter2value",
        ];
        let raw = format!(
            "\u{1b}[31merror\u{1b}[0m: {{\"accessToken\":\"{}\",\"refreshToken\":\"{}\"}} \
             Authorization: Bearer {} opaque {} password=hunter2value",
            secrets[0], secrets[3], secrets[1], secrets[2]
        );
        let redacted = redact_diagnostic(&raw);
        for secret in secrets {
            assert!(!redacted.contains(secret), "{secret} leaked: {redacted}");
        }
        assert!(!redacted.contains('\u{1b}'));
        assert!(redacted.contains("[redacted]"));

        let ordinary = "No such file or directory (os error 2)";
        assert_eq!(redact_diagnostic(ordinary), ordinary);

        let long = "word ".repeat(1_000);
        assert!(redact_diagnostic(&long).chars().count() <= MAX_DIAGNOSTIC_CHARS + 1);
    }

    #[cfg(not(windows))]
    fn shell_command(dir: &Path, script: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[cfg(not(windows))]
    #[test]
    fn failed_exit_reports_only_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = shell_command(
            dir.path(),
            "echo '{\"accessToken\":\"leaked-secret\"}'; echo leaked-secret >&2; exit 3",
        );
        let result = run_login_command(&mut command, Duration::from_secs(10), None);
        assert_eq!(result.outcome, ClaudeLoginOutcome::Failed(Some(3)));

        let mut ok = shell_command(dir.path(), "exit 0");
        let result = run_login_command(&mut ok, Duration::from_secs(10), None);
        assert_eq!(result.outcome, ClaudeLoginOutcome::Success);
    }

    #[cfg(not(windows))]
    #[test]
    fn cancel_and_timeout_kill_and_reap_the_child() {
        let dir = tempfile::tempdir().unwrap();

        let mut slow = shell_command(dir.path(), "exec sleep 30");
        let started = Instant::now();
        let result = run_login_command(&mut slow, Duration::from_millis(200), None);
        assert_eq!(result.outcome, ClaudeLoginOutcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));

        // A cancel requested before the child is bound must not be lost.
        let handle = ManagedLoginProcess::default();
        handle.cancel();
        let mut slow = shell_command(dir.path(), "exec sleep 30");
        let started = Instant::now();
        let result = run_login_command(&mut slow, Duration::from_secs(30), Some(&handle));
        assert_eq!(result.outcome, ClaudeLoginOutcome::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(10));

        // Cancel from another thread while the child is running.
        let handle = ManagedLoginProcess::default();
        let canceller = handle.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            canceller.cancel();
        });
        let mut slow = shell_command(dir.path(), "exec sleep 30");
        let started = Instant::now();
        let result = run_login_command(&mut slow, Duration::from_secs(30), Some(&handle));
        thread.join().unwrap();
        assert_eq!(result.outcome, ClaudeLoginOutcome::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(handle.take().is_none(), "child must be reaped");
    }

    #[cfg(windows)]
    #[test]
    fn abrupt_parent_exit_terminates_the_login_child() {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use std::os::windows::process::CommandExt;
        use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        const ROOT: &str = "CODEXBAR_CLAUDE_LOGIN_JOB_TEST_ROOT";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = PathBuf::from(root);
            let mut process = Command::new(which::which("powershell.exe").unwrap());
            process
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 30; exit 0",
                ])
                .current_dir(&root);
            isolate_account_subprocess_env(&mut process, &root);
            let login = spawn_login_process(&mut process).unwrap();
            std::fs::write(root.join("child.pid"), login.id.to_string()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !root.join("exit-now").exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(20));
            }
            // Deliberately bypass Drop: the OS must close the private job handle.
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        let parent = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "claude_accounts::login_runner::tests::abrupt_parent_exit_terminates_the_login_child",
                "--nocapture",
            ])
            .env(ROOT, dir.path())
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid_file = dir.path().join("child.pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pid_file.exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        // SAFETY: the helper reported its live child; this owned wait-only handle prevents PID reuse ambiguity.
        let process = unsafe {
            OwnedHandle::from_raw_handle(OpenProcess(PROCESS_SYNCHRONIZE, false, pid).unwrap().0)
        };
        std::fs::write(dir.path().join("exit-now"), "").unwrap();
        let output = parent.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            // SAFETY: valid wait-only process handle owned above.
            unsafe { WaitForSingleObject(HANDLE(process.as_raw_handle()), 5_000) },
            WAIT_OBJECT_0
        );
    }

    #[cfg(windows)]
    #[test]
    fn process_guard_distinguishes_native_cli_from_store_desktop() {
        use std::os::windows::process::CommandExt;
        for (product, expected) in [("Claude", "0"), ("Claude Code", "1")] {
            let script = format!(
                "function Get-Process {{ [pscustomobject]@{{ ProcessName='claude'; Path='C:\\fixture\\claude.exe'; MainModule=[pscustomobject]@{{FileVersionInfo=[pscustomobject]@{{ProductName='{product}'}}}} }} }}; {CLI_PROCESS_COUNT_SCRIPT}"
            );
            let output = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .env("CODEXBAR_CLAUDE_EXE", "C:\\different\\claude.exe")
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
    }
}
