//! Safe, user-facing categories of a failed "Add account" sign-in, plus the
//! WSL-backed configuration recovery guidance (port of upstream #480).
//!
//! Claude Code's raw child output is never captured (see `login_runner`);
//! only these categories, a process exit code, and a redacted launch
//! diagnostic reach the user.

use std::path::Path;

use super::file_locations::ambient_claude_config_dir;
use super::login_runner::ClaudeLoginOutcome;

/// Error text of a sign-in the user cancelled. Exported so callers can tell
/// an intentional cancel apart from a failure and stay quiet about it.
pub const SIGN_IN_CANCELLED_MESSAGE: &str = "Account setup cancelled.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoginFailure {
    Cancelled,
    MissingBinary,
    /// Payload is already redacted by `login_runner::redact_diagnostic`.
    LaunchFailed(String),
    TimedOut,
    CliExit(Option<i32>),
    CredentialsMissing,
    IdentityUnreadable,
}

impl LoginFailure {
    /// Map a login outcome to its failure category; `None` on success.
    pub(crate) fn from_outcome(outcome: &ClaudeLoginOutcome) -> Option<Self> {
        match outcome {
            ClaudeLoginOutcome::Success => None,
            ClaudeLoginOutcome::Cancelled => Some(Self::Cancelled),
            ClaudeLoginOutcome::MissingBinary => Some(Self::MissingBinary),
            ClaudeLoginOutcome::LaunchFailed(diagnostic) => {
                Some(Self::LaunchFailed(diagnostic.clone()))
            }
            ClaudeLoginOutcome::TimedOut => Some(Self::TimedOut),
            ClaudeLoginOutcome::Failed(code) => Some(Self::CliExit(*code)),
        }
    }

    /// User-facing message. `wsl_backed` swaps in the WSL recovery guidance
    /// for failures that a WSL-linked configuration plausibly explains;
    /// cancellation is user intent and never reworded as a WSL fault.
    pub(crate) fn message(&self, wsl_backed: bool) -> String {
        if wsl_backed && let Some(context) = self.wsl_context() {
            return wsl_failure_guidance(&context);
        }
        match self {
            Self::Cancelled => SIGN_IN_CANCELLED_MESSAGE.to_owned(),
            Self::MissingBinary => "The `claude` command could not be found.".to_owned(),
            Self::LaunchFailed(diagnostic) => {
                format!("Failed to start the Claude sign-in flow: {diagnostic}")
            }
            Self::TimedOut => "The Claude sign-in flow timed out.".to_owned(),
            Self::CliExit(Some(code)) => format!(
                "The Claude sign-in flow did not complete (Claude Code exit code {code}). \
                 Try again and finish sign-in in your browser."
            ),
            Self::CliExit(None) => "The Claude sign-in flow did not complete. \
                 Try again and finish sign-in in your browser."
                .to_owned(),
            Self::CredentialsMissing => {
                "Sign-in completed, but no credentials were written for this account.".to_owned()
            }
            Self::IdentityUnreadable => {
                "Sign-in completed, but the account identity could not be read.".to_owned()
            }
        }
    }

    fn wsl_context(&self) -> Option<String> {
        match self {
            Self::TimedOut => Some("the browser sign-in timed out".to_owned()),
            Self::CliExit(Some(code)) => Some(format!("Claude Code exit code {code}")),
            Self::CliExit(None) => Some("Claude Code exited unsuccessfully".to_owned()),
            Self::CredentialsMissing => Some("Claude Code saved no subscription login".to_owned()),
            Self::IdentityUnreadable => Some("the account identity could not be read".to_owned()),
            Self::Cancelled | Self::MissingBinary | Self::LaunchFailed(_) => None,
        }
    }
}

fn wsl_failure_guidance(context: &str) -> String {
    format!(
        "Claude sign-in did not complete ({context}). Your Claude configuration is a Windows \
         link into WSL, but Add account signs in to an isolated configuration directory and \
         does not write through that link. WSL2 and remote browser sign-in often cannot reach \
         Claude Code's local callback, so the sign-in code must be pasted into a terminal. Run \
         `claude auth login --claudeai` in your WSL terminal and finish signing in; CodexBar \
         then picks that account up automatically as your current Claude Code account. Or \
         install native Windows Claude Code and use Add account."
    )
}

/// UNC roots that identify a Windows path as living on the WSL filesystem.
const WSL_UNC_ROOTS: [&str; 2] = [r"\\wsl$\", r"\\wsl.localhost\"];

/// Lowercase a path for comparison, collapsing `/`, the `\\?\` verbatim
/// prefix, and the `\\?\UNC\` form so `\\wsl$\...` and `\\?\UNC\wsl$\...`
/// both match.
fn normalize_unc(value: &str) -> String {
    let slashes = value.replace('/', "\\");
    let without_verbatim = slashes.strip_prefix(r"\\?\").unwrap_or(&slashes);
    let mut normalized = without_verbatim.to_ascii_lowercase();
    if let Some(rest) = normalized.strip_prefix("unc\\") {
        normalized = format!(r"\\{rest}");
    }
    normalized
}

fn is_wsl_unc(path: &Path) -> bool {
    let normalized = normalize_unc(&path.to_string_lossy());
    WSL_UNC_ROOTS
        .iter()
        .any(|root| normalized.starts_with(*root))
}

fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

/// True when `path` is a link (symlink/junction) that resolves onto the WSL
/// filesystem, either directly or through the verbatim UNC form.
fn path_is_wsl_backed(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !is_link(&metadata) {
        return false;
    }
    std::fs::read_link(path).is_ok_and(|target| is_wsl_unc(&target))
        || path.canonicalize().is_ok_and(|target| is_wsl_unc(&target))
}

/// True when the ambient Claude config directory is a Windows link into WSL.
/// Checked only after sign-in fails, so Add account is never blocked
/// preemptively.
pub(crate) fn ambient_config_is_wsl_backed() -> bool {
    path_is_wsl_backed(&ambient_claude_config_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [LoginFailure; 8] = [
        LoginFailure::Cancelled,
        LoginFailure::MissingBinary,
        LoginFailure::LaunchFailed(String::new()),
        LoginFailure::TimedOut,
        LoginFailure::CliExit(Some(1)),
        LoginFailure::CliExit(None),
        LoginFailure::CredentialsMissing,
        LoginFailure::IdentityUnreadable,
    ];

    #[test]
    fn wsl_unc_paths_are_detected_and_native_paths_are_not() {
        for wsl in [
            r"\\wsl$\Ubuntu\home\user\.claude",
            r"\\wsl.localhost\Ubuntu\home\user\.claude",
            r"\\WSL.LOCALHOST\Ubuntu\home\user\.claude",
            r"\\?\UNC\wsl$\Ubuntu\home\user\.claude",
            r"//wsl$/Ubuntu/home/user/.claude",
        ] {
            assert!(is_wsl_unc(Path::new(wsl)), "{wsl}");
        }
        for native in [
            r"C:\Users\user\.claude",
            r"\\server\share\.claude",
            r"\\?\C:\Users\user\.claude",
            r"wsl$\Ubuntu\home\user\.claude",
            "/home/user/.claude",
        ] {
            assert!(!is_wsl_unc(Path::new(native)), "{native}");
        }
    }

    #[test]
    fn plain_directories_are_never_wsl_backed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!path_is_wsl_backed(dir.path()));
        assert!(!path_is_wsl_backed(&dir.path().join("missing")));
    }

    #[cfg(unix)]
    #[test]
    fn link_to_a_wsl_unc_target_is_wsl_backed_and_ordinary_links_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let wsl_link = dir.path().join("wsl-link");
        std::os::unix::fs::symlink(r"\\wsl$\Ubuntu\home\user\.claude", &wsl_link).unwrap();
        assert!(path_is_wsl_backed(&wsl_link));

        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let local_link = dir.path().join("local-link");
        std::os::unix::fs::symlink(&target, &local_link).unwrap();
        assert!(!path_is_wsl_backed(&local_link));
    }

    #[test]
    fn native_failures_keep_generic_guidance_and_a_safe_exit_code() {
        let exit = LoginFailure::CliExit(Some(3)).message(false);
        assert!(exit.contains("did not complete"), "{exit}");
        assert!(exit.contains("exit code 3"), "{exit}");
        for failure in &ALL {
            assert!(!failure.message(false).contains("WSL"), "{failure:?}");
        }
        assert_eq!(
            LoginFailure::Cancelled.message(false),
            "Account setup cancelled."
        );
        assert_eq!(
            LoginFailure::TimedOut.message(false),
            "The Claude sign-in flow timed out."
        );
    }

    #[test]
    fn wsl_failures_explain_isolated_sign_in_and_terminal_recovery() {
        for failure in [
            LoginFailure::CliExit(Some(1)),
            LoginFailure::CliExit(None),
            LoginFailure::TimedOut,
            LoginFailure::CredentialsMissing,
            LoginFailure::IdentityUnreadable,
        ] {
            let message = failure.message(true);
            assert!(message.contains("link into WSL"), "{message}");
            assert!(
                message.contains("isolated configuration directory"),
                "{message}"
            );
            assert!(
                message.contains("claude auth login --claudeai"),
                "{message}"
            );
            assert!(message.contains("automatically"), "{message}");
            assert!(message.contains("native Windows Claude Code"), "{message}");
        }
        // Cancelling is user intent; setup problems are not WSL faults.
        assert_eq!(
            LoginFailure::Cancelled.message(true),
            "Account setup cancelled."
        );
        assert!(!LoginFailure::MissingBinary.message(true).contains("WSL"));
    }

    #[test]
    fn failure_messages_never_echo_credential_material() {
        let leaked = super::super::login_runner::redact_diagnostic(
            "spawn failed: {\"accessToken\":\"sk-ant-oat01-secret0123456789\",\"refreshToken\":\"rt-secret\"} Bearer abc.def.ghi",
        );
        let mut failures = ALL.to_vec();
        failures.push(LoginFailure::LaunchFailed(leaked));
        for failure in failures {
            for wsl_backed in [false, true] {
                let message = failure.message(wsl_backed).to_ascii_lowercase();
                for marker in [
                    "sk-ant-",
                    "oat01",
                    "rt-secret",
                    "abc.def.ghi",
                    "claudeaioauth",
                ] {
                    assert!(!message.contains(marker), "{failure:?}: {message}");
                }
            }
        }
    }

    #[test]
    fn outcomes_map_to_categories() {
        assert_eq!(
            LoginFailure::from_outcome(&ClaudeLoginOutcome::Success),
            None
        );
        assert_eq!(
            LoginFailure::from_outcome(&ClaudeLoginOutcome::Failed(Some(2))),
            Some(LoginFailure::CliExit(Some(2)))
        );
        assert_eq!(
            LoginFailure::from_outcome(&ClaudeLoginOutcome::Cancelled),
            Some(LoginFailure::Cancelled)
        );
    }

    #[cfg(windows)]
    #[test]
    fn ordinary_junction_is_not_treated_as_wsl_backed() {
        use std::os::windows::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::create_dir_all(&target).unwrap();
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "New-Item -ItemType Junction -Path $env:CODEXBAR_TEST_LINK -Target $env:CODEXBAR_TEST_TARGET | Out-Null",
            ])
            .env("CODEXBAR_TEST_LINK", &link)
            .env("CODEXBAR_TEST_TARGET", &target)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Failed to create the local junction fixture."
        );
        assert!(!path_is_wsl_backed(&link));
        assert!(!path_is_wsl_backed(&target));
    }
}
