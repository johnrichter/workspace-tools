//! Skip-set-aware recursive `.md` enumeration over the reachable set.
//!
//! This is the first real consumer of [`crate::skipset`]'s pruning API (see
//! that module's enumerator-drift contract): every directory this walk
//! would otherwise descend into, and every file it would otherwise return,
//! is checked against the skip-set before it's kept.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::skipset::is_skipped;

/// Recursively finds every `.md` file under `roots` (the reachable
/// directory set — see [`crate::reachable::reachable_set`]), pruning any
/// directory or file the skip-set excludes.
///
/// **Pruning:** a directory is skipped (never descended into) when its own
/// name matches [`crate::skipset::SKIPPED_DIR_COMPONENTS`] or its
/// reachable-root-relative path matches a skip-set glob (e.g. an external
/// clone under `reference-materials/code-repositories/`) — the glob check
/// at the directory level means a large protected clone is never walked at
/// all, not just filtered out file-by-file afterward.
///
/// **Path contract:** the skip-set is checked against each candidate's path
/// *relative to the root it was found under* (per-root, not relative to an
/// arbitrary common ancestor of `roots`) — this is the form
/// [`crate::skipset::is_skipped`] documents as its input contract.
///
/// **Errors:** an unreadable directory entry (permission denied, deleted
/// mid-walk) is silently skipped rather than failing the whole walk — this
/// function never panics and never returns an error for a single bad
/// entry.
///
/// **Determinism:** the result is fully sorted (lexicographic path order)
/// and deduplicated, so overlapping roots or filesystem-dependent directory
/// iteration order never change the output.
pub fn enumerate_markdown_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        let root: &Path = root.as_path();
        let walker = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(move |entry| {
                if entry.path() == root {
                    return true;
                }
                let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
                // One skip-set decision, no second exclusion list:
                // `is_skipped` prunes by directory-name component
                // (`.git`, `node_modules`, `.venv`) *and* by protected-tree
                // glob. Returning false for a directory here stops walkdir
                // descending into it at all, so a large protected clone is
                // never walked, not just filtered file-by-file afterward.
                !is_skipped(rel)
            });
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    files.dedup();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "content").unwrap();
    }

    #[test]
    fn finds_md_files_and_prunes_the_skip_set() {
        let root = TempDir::new().unwrap();
        touch(root.path(), "docs/a.md");
        touch(root.path(), "docs/b.md");
        touch(root.path(), "README.md");
        touch(root.path(), "src/main.rs");
        touch(root.path(), ".git/HEAD");
        touch(root.path(), ".git/objects/x.md");
        touch(root.path(), "node_modules/pkg/index.md");
        touch(root.path(), ".venv/lib/x.md");
        touch(
            root.path(),
            "reference-materials/code-repositories/clone/README.md",
        );

        let found = enumerate_markdown_files(&[root.path().to_path_buf()]);
        let rel: Vec<PathBuf> = found
            .iter()
            .map(|p| p.strip_prefix(root.path()).unwrap().to_path_buf())
            .collect();

        assert_eq!(
            rel,
            vec![
                PathBuf::from("README.md"),
                PathBuf::from("docs/a.md"),
                PathBuf::from("docs/b.md"),
            ]
        );
    }

    #[test]
    fn result_is_sorted_and_deduplicated_across_overlapping_roots() {
        let root = TempDir::new().unwrap();
        touch(root.path(), "z.md");
        touch(root.path(), "a.md");
        touch(root.path(), "sub/m.md");

        // Two roots that overlap (repo root and a subdirectory of it) must
        // not produce duplicate entries for the file(s) reachable from both.
        let found = enumerate_markdown_files(&[root.path().to_path_buf(), root.path().join("sub")]);
        let rel: Vec<PathBuf> = found
            .iter()
            .map(|p| p.strip_prefix(root.path()).unwrap_or(p).to_path_buf())
            .collect();
        assert_eq!(
            rel,
            vec![
                PathBuf::from("a.md"),
                PathBuf::from("sub/m.md"),
                PathBuf::from("z.md")
            ]
        );
    }

    #[test]
    fn nonexistent_root_is_skipped_without_panicking() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("does-not-exist");
        let found = enumerate_markdown_files(&[missing]);
        assert!(found.is_empty());
    }

    #[test]
    fn non_md_files_are_never_returned() {
        let root = TempDir::new().unwrap();
        touch(root.path(), "notes.txt");
        touch(root.path(), "README");
        let found = enumerate_markdown_files(&[root.path().to_path_buf()]);
        assert!(found.is_empty());
    }

    #[test]
    fn empty_roots_list_yields_empty_result() {
        let found = enumerate_markdown_files(&[]);
        assert!(found.is_empty());
    }

    #[test]
    fn md_file_directly_under_an_excluded_dir_is_not_enumerated() {
        // Not just "excluded dir is pruned before descending further" (the
        // `finds_md_files_and_prunes_the_skip_set` case already covers a
        // nested file) — a `.md` sitting immediately inside the excluded
        // dir itself, one level deep, must also never appear.
        let root = TempDir::new().unwrap();
        touch(root.path(), "node_modules/x.md");
        touch(root.path(), ".venv/x.md");
        touch(root.path(), ".git/x.md");
        let found = enumerate_markdown_files(&[root.path().to_path_buf()]);
        assert!(found.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycle_does_not_hang_or_panic() {
        // A directory symlink pointing back at an ancestor creates a cycle
        // if followed. `follow_links(false)` means walkdir must treat the
        // symlink itself as a leaf (never descend through it), so the walk
        // terminates rather than looping forever.
        let root = TempDir::new().unwrap();
        touch(root.path(), "docs/a.md");
        let cycle_link = root.path().join("docs/loop");
        std::os::unix::fs::symlink(root.path(), &cycle_link).unwrap();

        let found = enumerate_markdown_files(&[root.path().to_path_buf()]);
        let rel: Vec<PathBuf> = found
            .iter()
            .map(|p| p.strip_prefix(root.path()).unwrap().to_path_buf())
            .collect();
        assert_eq!(rel, vec![PathBuf::from("docs/a.md")]);
    }
}
