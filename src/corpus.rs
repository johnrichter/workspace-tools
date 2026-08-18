//! The scanned corpus every subcommand reads from: resolve the reachable
//! directory set, then walk it into the freshness-cached `.md` scan.
//!
//! This is the one place [`crate::reachable`], [`crate::scan`], and the
//! freshness [`crate::cache`] are wired together, so `main` hands each
//! subcommand a ready `Vec<ScannedFile>` and never touches the walk itself.
//! The cache is an accelerator: a failure to persist it degrades the next
//! run to a cold rebuild, never this one.

use std::path::{Path, PathBuf};

use logkit::Logger;

use crate::cache::FreshnessCache;
use crate::reachable::{self, ScopePaths};
use crate::scan::{self, ScannedFile};

/// Gathers the scanned corpus for a run launched in `cwd`, extending the
/// reachable set with `extra_dirs` (the subcommand's `--dir` flags).
///
/// Every non-fatal hiccup -- an unresolvable reachable set, an unpersistable
/// cache -- is narrated to `logger` and worked around, never fatal: a run
/// that reaches this point always gets a corpus to search over.
pub fn gather(cwd: &Path, extra_dirs: &[PathBuf], logger: &Logger) -> Vec<ScannedFile> {
    let reachable_dirs = reachable::reachable_set(cwd, &ScopePaths::for_current_os(), extra_dirs)
        .unwrap_or_else(|err| {
            let _ = logger
                .warn(format!(
                    "could not resolve the reachable directory set ({err}); limiting this run to the current directory"
                ))
                .emit();
            vec![cwd.to_path_buf()]
        });

    let mut cache = FreshnessCache::open(cwd);
    let scanned = scan::scan(&reachable_dirs, &mut cache);
    if let Err(err) = cache.save() {
        let _ = logger
            .warn(format!("failed to persist freshness cache: {err}"))
            .emit();
    }
    scanned
}
