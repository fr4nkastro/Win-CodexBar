//! Windows Claude Desktop project-root discovery.
//!
//! Claude Desktop stores local-agent session work below its Electron
//! application-data directory. The layout has varied by release, so discovery
//! walks only the two known session roots to a shallow fixed depth and returns
//! nested `.claude/projects` directories. Every directory/type/path operation
//! shares the caller's metadata scan budget.

use super::pi_family::DirectoryScanBudget;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const SESSION_DIRECTORY_NAMES: [&str; 2] = ["local-agent-mode-sessions", "claude-code-sessions"];
const MAX_DEPTH: usize = 4;

const SKIPPED_DIRECTORY_NAMES: [&str; 7] = [
    ".build",
    ".git",
    "build",
    "DerivedData",
    "node_modules",
    "outputs",
    "target",
];

pub(crate) struct ClaudeDesktopProjectsLocator;

impl ClaudeDesktopProjectsLocator {
    /// Locate Claude Desktop roots in the current Windows user's AppData.
    ///
    /// macOS/Linux builds deliberately return no roots: their Claude Desktop
    /// layouts are not Windows paths and are handled by their native ports.
    #[cfg(windows)]
    pub(crate) fn roots(budget: &mut DirectoryScanBudget) -> Vec<PathBuf> {
        let Some(data_directory) = dirs::data_dir() else {
            return Vec::new();
        };
        Self::roots_under(&data_directory.join("Claude"), budget)
    }

    #[cfg(not(windows))]
    pub(crate) fn roots(_budget: &mut DirectoryScanBudget) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Discover roots below an injected application-data directory.
    ///
    /// This platform-neutral helper keeps the traversal deterministic and
    /// makes the Windows deadline contract testable without touching a real
    /// user profile.
    pub(crate) fn roots_under(
        application_data_root: &Path,
        budget: &mut DirectoryScanBudget,
    ) -> Vec<PathBuf> {
        if !budget.has_time_remaining() {
            return Vec::new();
        }

        let session_roots = SESSION_DIRECTORY_NAMES
            .into_iter()
            .map(|name| application_data_root.join(name))
            .collect::<Vec<_>>();
        let mut queue = session_roots
            .iter()
            .cloned()
            .map(|path| (path, 0_usize))
            .collect::<Vec<_>>();
        let mut visited = session_roots
            .iter()
            .map(|path| visited_key(path))
            .collect::<HashSet<_>>();
        let mut roots = Vec::new();
        let mut next_index = 0;

        while next_index < queue.len() && budget.has_time_remaining() {
            let (current, depth) = {
                let (path, depth) = &queue[next_index];
                (path.clone(), *depth)
            };
            next_index += 1;

            let projects = current.join(".claude").join("projects");
            if budget.has_time_remaining()
                && projects.is_dir()
                && let Some(canonical) = budget.canonicalize_if_time_remaining(&projects)
            {
                roots.push(canonical);
            }

            if depth >= MAX_DEPTH || depth >= budget.max_depth() || !budget.has_time_remaining() {
                continue;
            }

            let entries = budget.child_directories(&current);
            let children = budget.compact_map_while_time_remaining(entries, |entry| {
                let name = entry.file_name();
                if is_skipped_directory_name(&name.to_string_lossy()) {
                    return None;
                }
                let canonical = budget.canonicalize_if_time_remaining(&entry.path())?;
                let key = visited_key(&canonical);
                visited.insert(key).then_some(canonical)
            });
            queue.extend(children.into_iter().map(|path| (path, depth + 1)));
        }

        roots.sort();
        roots.dedup();
        roots
    }
}

fn is_skipped_directory_name(name: &str) -> bool {
    SKIPPED_DIRECTORY_NAMES
        .iter()
        .any(|skipped| skipped.eq_ignore_ascii_case(name))
}

fn visited_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        value.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        value
    }
}
