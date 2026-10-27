//! Log/error-screenshot retention (direction-7a, 2026-10-28 decision):
//! user-settable numeric budget — days AND gigabytes, 0 = that dimension
//! unlimited, defaults 14 days / 5 GB — enforced by the LAUNCHER (this
//! module), so the payload repo carries zero diff for cleanup.
//!
//! What gets pruned (payload-root `log/`, cwd-relative like deploy_config):
//! - flat day logs `log/YYYY-MM-DD_<instance>.txt` — dated by the FILENAME
//!   prefix, never mtime: the 2026-10-24 shell swap rsync clobbered mtimes,
//!   and filename dates are what the webui shows anyway.
//! - error screenshot dirs `log/error/<id>/` — dated by the inner
//!   `YYYY-MM-DD_HH-MM-SS-*.png` filename (mtime fallback), deleted whole.
//!
//! Anything that doesn't match those shapes is never touched.
//!
//! Policy application (pure core, unit-tested): the age pass deletes every
//! entry older than `retention_days`; the size pass then deletes
//! oldest-first until the remaining total fits `size_limit_gb`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use crate::shell_settings::ShellSettings;

/// Interval between enforcement passes (the settings file is re-read every
/// cycle, so webui edits apply within one cycle at worst).
const CLEANUP_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// Startup delay: let the backend come up before the first pruning pass.
const CLEANUP_STARTUP_DELAY: Duration = Duration::from_secs(30);

/// One prunable item, already resolved to an age and a size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanupEntry {
    /// Display/delete key (absolute path in production, arbitrary in tests).
    pub rel_path: String,
    /// Days old per the entry's own filename date (mtime fallback).
    pub days_old: u64,
    pub size_bytes: u64,
    /// Error-screenshot dirs are removed recursively; txt files are unlinked.
    pub is_dir: bool,
}

/// Which entries the policy deletes. Pure.
///
/// Age pass: `days_old > retention_days` (0 disables). Size pass: after the
/// age pass, delete oldest-first until the remaining total is within the
/// budget (0 disables); ties break by path so plans are deterministic.
pub(crate) fn prune_plan(
    entries: Vec<CleanupEntry>,
    retention_days: u64,
    size_limit_gb: f64,
) -> Vec<CleanupEntry> {
    // Age pass: partition — over-retention entries are deleted immediately.
    let mut kept: Vec<CleanupEntry> = Vec::new();
    let mut deleted: Vec<CleanupEntry> = entries
        .into_iter()
        .filter(|e| {
            let expired = retention_days != 0 && e.days_old > retention_days;
            if !expired {
                kept.push(e.clone());
            }
            expired
        })
        .collect();
    if size_limit_gb > 0.0 {
        // Victim order (out the door first): oldest, ties broken
        // alphabetically — i.e. sort kept ascending-by-age with reversed path
        // tie-break, so `pop()` yields the deletion order above.
        kept.sort_by(|a, b| a.days_old.cmp(&b.days_old).then(b.rel_path.cmp(&a.rel_path)));
        let limit_bytes = size_limit_gb * 1024.0 * 1024.0 * 1024.0;
        let mut total: f64 = kept.iter().map(|e| e.size_bytes as f64).sum();
        while total > limit_bytes {
            let Some(victim) = kept.pop() else {
                break;
            };
            total -= victim.size_bytes as f64;
            deleted.push(victim);
        }
    }
    deleted
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm — no
/// chrono dependency for one subtraction).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse `YYYY-MM-DD` (10 chars) at the start of a name → civil-day number.
fn civil_days_from_prefix(name: &str) -> Option<i64> {
    let bytes = name.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let digits: Option<[i64; 3]> = Some([
        name[0..4].parse().ok()?,
        name[5..7].parse().ok()?,
        name[8..10].parse().ok()?,
    ]);
    let [y, m, d] = digits?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

fn today_civil_days(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

fn days_old_from_prefix(name: &str, today: i64) -> Option<u64> {
    let days = civil_days_from_prefix(name)?;
    (today - days).try_into().ok()
}

/// Recursive byte size (bounded by reality: error dirs hold a handful of
/// PNGs). Unreadable entries count 0 — an unmeasurable dir must not block
/// pruning the rest.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += dir_size(&entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// Scan `log/` and build the entry set (never touches anything else).
fn scan_entries(log_root: &Path, today: i64) -> Vec<CleanupEntry> {
    let mut entries = Vec::new();
    let Ok(read) = std::fs::read_dir(log_root) else {
        return entries;
    };
    for entry in read.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if meta.is_dir() {
            if name != "error" {
                continue;
            }
            let Ok(errors) = std::fs::read_dir(entry.path()) else { continue };
            for error_dir in errors.flatten() {
                let Ok(dir_meta) = error_dir.metadata() else { continue };
                if !dir_meta.is_dir() {
                    continue;
                }
                let dir_name = error_dir.file_name().to_string_lossy().into_owned();
                // Date from the first dated inner file; mtime fallback.
                let age = std::fs::read_dir(error_dir.path())
                    .ok()
                    .and_then(|inner| {
                        inner
                            .flatten()
                            .map(|f| f.file_name().to_string_lossy().into_owned())
                            .find_map(|n| days_old_from_prefix(&n, today))
                    })
                    .or_else(|| {
                        // mtime fallback: AGE = today-day − mtime-day (a bare
                        // epoch/86400 would read "days since 1970" and nuke
                        // every fresh error dir).
                        dir_meta
                            .modified()
                            .ok()
                            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                            .and_then(|d| u64::try_from(today - (d.as_secs() / 86_400) as i64).ok())
                    });
                let Some(days_old) = age else { continue };
                entries.push(CleanupEntry {
                    rel_path: format!("error/{dir_name}"),
                    days_old,
                    size_bytes: dir_size(&error_dir.path()),
                    is_dir: true,
                });
            }
        } else if let Some(days_old) = days_old_from_prefix(&name, today) {
            entries.push(CleanupEntry {
                rel_path: name,
                days_old,
                size_bytes: meta.len(),
                is_dir: false,
            });
        }
    }
    entries
}

/// One enforcement pass over `log_root` (absolute in tests, `./log` in
/// production). Returns (deleted count, freed bytes).
pub(crate) fn run_once(log_root: &Path, settings: &ShellSettings, now: SystemTime) -> (usize, u64) {
    if !log_root.exists() {
        return (0, 0);
    }
    let today = today_civil_days(now);
    let entries = scan_entries(log_root, today);
    let plan = prune_plan(
        entries,
        u64::from(settings.log_retention_days),
        settings.log_size_limit_gb,
    );
    let mut deleted = 0usize;
    let mut freed = 0u64;
    for entry in &plan {
        let path: PathBuf = if entry.is_dir {
            log_root.join("error").join(&entry.rel_path["error/".len()..])
        } else {
            log_root.join(&entry.rel_path)
        };
        let result = if entry.is_dir {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => {
                deleted += 1;
                freed += entry.size_bytes;
            }
            // The backend may hold an old log open — skip, next cycle retries.
            Err(e) => warn!("log cleanup: failed to remove {}: {e}", entry.rel_path),
        }
    }
    if deleted > 0 {
        info!(
            "log cleanup: removed {deleted} entries ({:.1} MB freed) per retention {}d / {}GB",
            freed as f64 / 1_048_576.0,
            settings.log_retention_days,
            settings.log_size_limit_gb
        );
    }
    (deleted, freed)
}

/// Background enforcement thread: one pass after startup, then every
/// [`CLEANUP_INTERVAL`]; settings re-read from disk each cycle so webui
/// edits apply without a restart. cwd-relative `./log` — the launcher
/// process runs in the payload root (same convention as deploy_config).
pub fn spawn_cleanup_thread() {
    std::thread::Builder::new()
        .name("log-cleanup".into())
        .spawn(|| {
            std::thread::sleep(CLEANUP_STARTUP_DELAY);
            loop {
                let settings = crate::shell_settings::load();
                run_once(Path::new("./log"), &settings, SystemTime::now());
                std::thread::sleep(CLEANUP_INTERVAL);
            }
        })
        .expect("spawn log-cleanup thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, days: u64, size: u64, is_dir: bool) -> CleanupEntry {
        CleanupEntry {
            rel_path: path.into(),
            days_old: days,
            size_bytes: size,
            is_dir,
        }
    }

    #[test]
    fn age_pass_deletes_only_over_retention() {
        let entries = vec![entry("2026-01-01_a.txt", 30, 100, false), entry("today_b.txt", 0, 100, false)];
        let plan = prune_plan(entries, 14, 0.0);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].rel_path, "2026-01-01_a.txt");
    }

    #[test]
    fn zero_retention_disables_age_pass() {
        let entries = vec![entry("ancient.txt", 3650, 1, false)];
        assert!(prune_plan(entries, 0, 0.0).is_empty());
    }

    #[test]
    fn size_pass_deletes_oldest_first_until_under_budget() {
        let gb = 1024.0 * 1024.0 * 1024.0;
        let entries = vec![
            entry("new.txt", 1, gb as u64, false),
            entry("mid.txt", 5, gb as u64, false),
            entry("old.txt", 9, gb as u64, false),
        ];
        // 2 GB budget over 3 GB of data: the oldest goes, newest stays.
        let plan = prune_plan(entries, 0, 2.0);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].rel_path, "old.txt");
    }

    #[test]
    fn size_pass_tie_breaks_by_path_deterministically() {
        let gb = 1024.0 * 1024.0 * 1024.0;
        let entries = vec![
            entry("b_same_age.txt", 3, gb as u64, false),
            entry("a_same_age.txt", 3, gb as u64, false),
        ];
        // 1.5 GB budget over 2 GB of same-age data: one deletion, the
        // alphabetically-first path.
        let plan = prune_plan(entries, 0, 1.5);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].rel_path, "a_same_age.txt");
    }

    #[test]
    fn both_passes_compose() {
        let gb = 1024.0 * 1024.0 * 1024.0;
        let entries = vec![
            entry("ancient.txt", 100, 1, false),          // age pass takes it
            entry("old_but_kept.txt", 10, gb as u64, false), // size pass takes it (older)
            entry("new.txt", 1, gb as u64, false),           // stays: 1 GB ≤ budget
        ];
        let mut plan = prune_plan(entries, 14, 1.0);
        plan.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].rel_path, "ancient.txt");
        assert_eq!(plan[1].rel_path, "old_but_kept.txt");
    }

    #[test]
    fn civil_day_math_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2026, 10, 28) - days_from_civil(2026, 10, 1), 27);
    }

    #[test]
    fn prefix_parsing_accepts_real_names_rejects_others() {
        assert!(civil_days_from_prefix("2026-10-27_alas.txt").is_some());
        assert!(civil_days_from_prefix("2026-10-24_20-58-06-149442.png").is_some());
        assert!(civil_days_from_prefix("1792846686218").is_none());
        assert!(civil_days_from_prefix("log.txt").is_none());
        assert!(civil_days_from_prefix("2026-13-99_x.txt").is_none());
    }

    #[test]
    fn days_old_never_negative_on_clock_skew() {
        // A future-dated file must not underflow u64 — it just reads age 0
        // (try_into on negative fails → entry skipped as undatable, fine).
        let today = days_from_civil(2026, 10, 28);
        assert_eq!(days_old_from_prefix("2026-10-29_future.txt", today), None);
        assert_eq!(
            days_old_from_prefix("2026-10-28_today.txt", today),
            Some(0)
        );
    }

    #[test]
    fn run_once_prunes_realistic_tree() {
        let root = std::env::temp_dir().join(format!("alas-log-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mk = |p: &str, contents: &str| {
            let full = root.join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        };
        mk("2026-09-01_alas.txt", "old day log");
        mk("2099-01-01_alas.txt", "future log stays");
        mk("notadate.txt", "never touched");
        mk("error/1111/2026-09-01_01-02-03-4.png", "x");
        mk("error/2222/2099-01-01_01-02-03-4.png", "x");
        let settings = ShellSettings {
            log_retention_days: 14,
            log_size_limit_gb: 0.0,
            ..Default::default()
        };
        // "today" = 2026-10-28: the 09-01 pair is >14d old, 2099 pair stays.
        let now = UNIX_EPOCH + Duration::from_secs(days_from_civil(2026, 10, 28) as u64 * 86_400);
        let (deleted, _) = run_once(&root, &settings, now);
        assert_eq!(deleted, 2, "one txt + one error dir");
        assert!(!root.join("2026-09-01_alas.txt").exists());
        assert!(!root.join("error/1111").exists());
        assert!(root.join("2099-01-01_alas.txt").exists());
        assert!(root.join("error/2222").exists());
        assert!(root.join("notadate.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
