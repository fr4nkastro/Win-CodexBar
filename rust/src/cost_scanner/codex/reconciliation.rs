use super::*;

pub(super) fn paused_codex_summary(
    cache: &CostUsageCache,
    start_date: NaiveDate,
    today: NaiveDate,
) -> CostSummary {
    let report = cache
        .previous_report
        .clone()
        .unwrap_or_else(|| JsonlScanner::cached_cost_report_from_days(cache));
    summary_from_cached_report(&report, start_date, today)
}

/// Return cached Codex files that are provably gone from the portion of the
/// sessions tree covered by this scan. Entries outside the current roots or
/// date directories are intentionally retained for a later scan.
pub(super) fn missing_codex_cache_paths(
    cache: &CostUsageCache,
    sessions_dirs: &[PathBuf],
    range: &CostUsageDayRange,
) -> Vec<String> {
    let scanned_date_dirs: Vec<PathBuf> = sessions_dirs
        .iter()
        .flat_map(|sessions_dir| {
            codex_scan_dates(range).into_iter().map(|date| {
                sessions_dir
                    .join(date.format("%Y").to_string())
                    .join(date.format("%m").to_string())
                    .join(date.format("%d").to_string())
            })
        })
        .collect();

    cache
        .files
        .keys()
        .filter(|path_key| {
            let path = Path::new(path_key.as_str());
            let in_scanned_root = sessions_dirs
                .iter()
                .any(|sessions_dir| path.starts_with(sessions_dir));
            let in_scanned_date = scanned_date_dirs
                .iter()
                .any(|date_dir| path.starts_with(date_dir));
            in_scanned_root && in_scanned_date && !path.exists()
        })
        .cloned()
        .collect()
}

/// Remove cached Codex files that are provably gone after an explicit refresh.
pub(super) fn reconcile_missing_codex_cache_files(
    cache: &mut CostUsageCache,
    sessions_dirs: &[PathBuf],
    range: &CostUsageDayRange,
) {
    for path in missing_codex_cache_paths(cache, sessions_dirs, range) {
        cache.files.remove(&path);
    }
    cache
        .codex_pending_paths
        .retain(|path| Path::new(path).exists());
}

/// Whether the cache contains a path that depends on this source partition.
/// Missing optional roots are normal; only a root/date partition that has
/// previously contributed a cached or queued path can make discovery
/// incomplete.
pub(super) fn cache_has_codex_path_under(cache: &CostUsageCache, parent: &Path) -> bool {
    cache
        .files
        .keys()
        .chain(cache.codex_pending_paths.iter())
        .any(|path| Path::new(path).starts_with(parent))
}
