//! The "walk + cache" entry point every subcommand (`search`/`find`/`lint`)
//! consumes: given the reachable roots, return every skip-set-permitted
//! `.md` file with its freshness-cached parsed frontmatter.
//!
//! No LLM, no network anywhere in this path — purely filesystem, hashing,
//! and parsing, and fully deterministic for a fixed set of files on disk.

use std::path::PathBuf;

use frontmatter::{FrontmatterParseError, ParsedFrontmatter};

use crate::cache::FreshnessCache;
use crate::mdwalk;

/// One `.md` file found in the reachable set, with its parse outcome.
///
/// `parsed` is `Err` only for a document whose frontmatter itself doesn't
/// parse (malformed YAML, unclosed delimiter, ...) — that's a property of
/// the document, not a scan failure, so the file still appears in the
/// result set for a later lint pass to report on, rather than being
/// silently dropped.
pub struct ScannedFile {
    pub path: PathBuf,
    pub parsed: Result<ParsedFrontmatter, FrontmatterParseError>,
}

/// Enumerates every skip-set-permitted `.md` file under `roots` (the
/// reachable directory set — see [`crate::reachable::reachable_set`]) and
/// returns each with its freshness-cached parsed frontmatter, in the
/// walk's deterministic path order.
///
/// `cache` is opened by the caller ([`FreshnessCache::open`] in production,
/// [`FreshnessCache::open_in`] in tests) rather than by this function, so a
/// caller can reuse one cache across multiple scans in a process and
/// decide exactly when to persist it via [`FreshnessCache::save`].
///
/// A file this scan can't even stat or read (permission error, deleted
/// mid-walk) is silently dropped from the result rather than failing the
/// whole scan over one bad file.
pub fn scan(roots: &[PathBuf], cache: &mut FreshnessCache) -> Vec<ScannedFile> {
    mdwalk::enumerate_markdown_files(roots)
        .into_iter()
        .filter_map(|path| {
            let parsed = cache.get_or_parse(&path).ok()?;
            Some(ScannedFile { path, parsed })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_md(root: &std::path::Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }

    #[test]
    fn scans_reachable_roots_and_serves_parsed_frontmatter() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_md(repo.path(), "a.md", "---\nname: \"A\"\n---\nbody\n");
        write_md(
            repo.path(),
            ".git/x.md",
            "---\nname: \"Hidden\"\n---\nbody\n",
        );

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        let results = scan(&[repo.path().to_path_buf()], &mut cache);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, repo.path().join("a.md"));
        assert_eq!(
            results[0].parsed.as_ref().unwrap().name,
            Some("A".to_string())
        );
    }

    #[test]
    fn malformed_file_is_included_with_its_parse_error_not_dropped() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        write_md(repo.path(), "bad.md", "---\ntags: [unclosed\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        let results = scan(&[repo.path().to_path_buf()], &mut cache);

        assert_eq!(results.len(), 1);
        assert!(results[0].parsed.is_err());
    }
}
