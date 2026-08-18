//! Out-of-tree freshness cache for parsed `.md` frontmatter.
//!
//! # Freshness protocol
//! - **fast path** — unchanged mtime + size: reuse the cached `parsed`
//!   result, no reparse and no rehash.
//! - **confirm** — mtime or size changed: recompute the file's SHA-256.
//!   Unchanged hash (a touch-without-edit) still reuses `parsed`, only the
//!   fingerprint is refreshed; a changed hash reparses just this file.
//! - **miss** — no cache entry at all: reparse.
//!
//! # Accelerator, never a source of truth
//! Deleting the cache file reproduces byte-identical results on the next
//! call — a full cold rebuild straight from the files on disk. This cache
//! only ever saves reparse work; it never changes what a scan finds or how
//! a file's frontmatter is interpreted.
//!
//! # Corruption
//! A cache file that fails to deserialize — truncated write, disk
//! corruption, or a shape written by a future navigator version — is
//! treated as an empty cache (every file becomes a cold-parse miss), never
//! a hard error or a panic. Cache-format evolution is this module's own
//! problem, not a compatibility contract with any other consumer.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use frontmatter::{FrontmatterParseError, ParsedFrontmatter};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One cached file's freshness fingerprint plus its parsed frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    abs_path: PathBuf,
    mtime_secs: u64,
    mtime_nanos: u32,
    size: u64,
    content_sha256: String,
    parsed: ParsedFrontmatter,
}

/// On-disk cache shape: a flat, path-sorted list rather than a map, so the
/// persisted JSON never depends on hash-map iteration order — see
/// [`FreshnessCache::save`].
#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    entries: Vec<CacheEntry>,
}

/// A repo-scoped, out-of-tree cache of parsed `.md` frontmatter, keyed by
/// absolute file path, persisted as JSON under the platform cache dir.
///
/// Never a source of truth: see the module doc's accelerator and
/// corruption contracts.
pub struct FreshnessCache {
    path: PathBuf,
    entries: HashMap<PathBuf, CacheEntry>,
}

impl FreshnessCache {
    /// Opens the cache for `repo_root` at its real platform cache location
    /// (see [`cache_base_dir`]). Production entry point.
    pub fn open(repo_root: &Path) -> Self {
        Self::open_in(&cache_base_dir(), repo_root)
    }

    /// Opens the cache for `repo_root` rooted at `cache_base` instead of the
    /// real platform cache dir — the hermetic entry point tests use so a
    /// test run never reads or writes the operator's real cache.
    pub fn open_in(cache_base: &Path, repo_root: &Path) -> Self {
        let path = cache_file_path(cache_base, repo_root);
        let entries = read_cache_file(&path);
        Self { path, entries }
    }

    /// Returns the fresh, parsed frontmatter for `abs_path`, served from
    /// cache when the freshness protocol confirms it's still valid,
    /// otherwise (re)parsed with the in-memory cache entry updated —
    /// [`Self::save`] persists the update to disk.
    ///
    /// The outer [`io::Result`] is only for I/O failures reading `abs_path`
    /// itself (missing, permission denied); the inner `Result` is the
    /// document's own parse outcome. A file whose frontmatter fails to
    /// parse is not an I/O failure and is never cached as a hit — the next
    /// call reparses it, so a subsequent fix to the file is picked up
    /// immediately rather than being masked by a cached failure.
    pub fn get_or_parse(
        &mut self,
        abs_path: &Path,
    ) -> io::Result<Result<ParsedFrontmatter, FrontmatterParseError>> {
        let metadata = std::fs::metadata(abs_path)?;
        let (mtime_secs, mtime_nanos) = fingerprint_mtime(&metadata);
        let size = metadata.len();

        if let Some(cached) = self.entries.get(abs_path) {
            if cached.mtime_secs == mtime_secs
                && cached.mtime_nanos == mtime_nanos
                && cached.size == size
            {
                return Ok(Ok(cached.parsed.clone()));
            }
        }

        let bytes = std::fs::read(abs_path)?;
        let content_sha256 = sha256_hex(&bytes);

        if let Some(cached) = self.entries.get(abs_path) {
            if cached.content_sha256 == content_sha256 {
                let parsed = cached.parsed.clone();
                self.entries.insert(
                    abs_path.to_path_buf(),
                    CacheEntry {
                        abs_path: abs_path.to_path_buf(),
                        mtime_secs,
                        mtime_nanos,
                        size,
                        content_sha256,
                        parsed: parsed.clone(),
                    },
                );
                return Ok(Ok(parsed));
            }
        }

        // Frontmatter is defined over text; a non-UTF-8 byte sequence is
        // decoded lossily rather than treated as an I/O failure, so a
        // stray invalid byte in the body doesn't take the whole file out
        // of the scan.
        let text = String::from_utf8_lossy(&bytes);
        match frontmatter::parse(&text) {
            Ok(parsed) => {
                self.entries.insert(
                    abs_path.to_path_buf(),
                    CacheEntry {
                        abs_path: abs_path.to_path_buf(),
                        mtime_secs,
                        mtime_nanos,
                        size,
                        content_sha256,
                        parsed: parsed.clone(),
                    },
                );
                Ok(Ok(parsed))
            }
            Err(err) => {
                self.entries.remove(abs_path);
                Ok(Err(err))
            }
        }
    }

    /// Persists the in-memory cache to disk as JSON, sorted by `abs_path`
    /// so repeated saves of the same logical content are byte-identical.
    ///
    /// A failure to create the cache directory or write the file is
    /// reported to the caller rather than panicking — this cache is an
    /// accelerator, so a caller may reasonably log and continue with an
    /// in-memory-only cache for the rest of the run.
    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut entries: Vec<CacheEntry> = self.entries.values().cloned().collect();
        entries.sort_by(|a, b| a.abs_path.cmp(&b.abs_path));
        let json = serde_json::to_string(&CacheFile { entries })
            .expect("CacheFile of already-Serialize types is always serializable");
        std::fs::write(&self.path, json)
    }

    /// The number of entries currently held in memory (post any
    /// [`Self::get_or_parse`] calls this run, pre or post [`Self::save`]).
    /// Exposed for callers/tests that want to assert on cache size without
    /// reaching into the private entry map.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// The on-disk location this cache reads from and writes to. Exposed
    /// for tests that exercise cache-file corruption directly.
    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }
}

/// The real platform cache base directory, with `navigator` appended.
/// Production callers use [`FreshnessCache::open`], which calls this;
/// [`FreshnessCache::open_in`] lets tests substitute a temp dir instead so
/// no test run ever touches the operator's real cache.
fn cache_base_dir() -> PathBuf {
    let base = dirs::cache_dir()
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"));
    base.join("navigator")
}

/// The cache file for `repo_root`: named by the SHA-256 hex of its
/// absolute path, so distinct repos — including two clones of the same
/// repo at different locations — never collide on one cache file.
fn cache_file_path(cache_base: &Path, repo_root: &Path) -> PathBuf {
    let abs = std::path::absolute(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let key = sha256_hex(abs.to_string_lossy().as_bytes());
    cache_base.join(format!("{key}.json"))
}

/// Reads and deserializes the cache file at `path`. Any failure — missing
/// file, unreadable, corrupt/foreign JSON — yields an empty cache (every
/// file becomes a cold-parse miss) rather than an error; see the module
/// doc's corruption contract.
fn read_cache_file(path: &Path) -> HashMap<PathBuf, CacheEntry> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(file) = serde_json::from_str::<CacheFile>(&contents) else {
        return HashMap::new();
    };
    file.entries
        .into_iter()
        .map(|entry| (entry.abs_path.clone(), entry))
        .collect()
}

/// `(secs_since_epoch, subsec_nanos)` for `metadata`'s modified time.
/// Stored as two plain integers rather than persisting [`std::time::SystemTime`]
/// directly, so the cache's JSON shape doesn't depend on serde's
/// `SystemTime` wire format. A modified time this crate can't determine
/// (platform without mtime support, or a time before the Unix epoch)
/// fingerprints as zero, which simply forces a rehash-confirm on the next
/// read rather than panicking.
fn fingerprint_mtime(metadata: &std::fs::Metadata) -> (u64, u32) {
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    let since_epoch = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since_epoch.as_secs(), since_epoch.subsec_nanos())
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    fn write_md(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn cold_miss_parses_and_populates_the_cache() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(&file, "---\nname: \"A\"\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        let parsed = cache.get_or_parse(&file).unwrap().unwrap();
        assert_eq!(parsed.name, Some("A".to_string()));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn fast_path_reuses_cached_parse_without_reparsing_changed_bytes() {
        // Mutate the cache's stored `parsed` value directly (bypassing a
        // real reparse) after the first call, without touching the file on
        // disk (mtime/size stay identical). If the fast path skips
        // reparsing as documented, the second call must return the
        // mutated value, not a value that matches the file's real content.
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(&file, "---\nname: \"Real\"\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        cache.get_or_parse(&file).unwrap().unwrap();

        let sentinel = cache.entries.get_mut(&file).unwrap();
        sentinel.parsed.name = Some("Sentinel".to_string());

        let second = cache.get_or_parse(&file).unwrap().unwrap();
        assert_eq!(
            second.name,
            Some("Sentinel".to_string()),
            "fast path must reuse the cached value verbatim, proving it never reparsed"
        );
    }

    #[test]
    fn touch_without_edit_reuses_cached_parse_but_refreshes_fingerprint() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(&file, "---\nname: \"A\"\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        cache.get_or_parse(&file).unwrap().unwrap();
        let original_mtime = cache.entries.get(&file).unwrap().mtime_secs;

        // Bump mtime forward without changing content.
        let new_mtime = std::time::SystemTime::now() + Duration::from_mins(2);
        let file_handle = fs::File::open(&file).unwrap();
        file_handle.set_modified(new_mtime).unwrap();

        let sentinel = cache.entries.get_mut(&file).unwrap();
        sentinel.parsed.name = Some("Sentinel".to_string());

        let second = cache.get_or_parse(&file).unwrap().unwrap();
        assert_eq!(
            second.name,
            Some("Sentinel".to_string()),
            "touch-without-edit must reuse the cached parse, not reparse"
        );
        let refreshed_mtime = cache.entries.get(&file).unwrap().mtime_secs;
        assert_ne!(
            refreshed_mtime, original_mtime,
            "the fingerprint itself must still be refreshed even though parsed is reused"
        );
    }

    #[test]
    fn single_file_edit_only_reparses_that_file() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file_a = repo.path().join("a.md");
        let file_b = repo.path().join("b.md");
        write_md(&file_a, "---\nname: \"A\"\n---\nbody\n");
        write_md(&file_b, "---\nname: \"B\"\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        cache.get_or_parse(&file_a).unwrap().unwrap();
        cache.get_or_parse(&file_b).unwrap().unwrap();

        // Sentinel-mark both cached entries, then edit only `a.md`.
        cache.entries.get_mut(&file_a).unwrap().parsed.name = Some("SentinelA".to_string());
        cache.entries.get_mut(&file_b).unwrap().parsed.name = Some("SentinelB".to_string());
        write_md(&file_a, "---\nname: \"A2\"\n---\nbody\n");

        let a = cache.get_or_parse(&file_a).unwrap().unwrap();
        let b = cache.get_or_parse(&file_b).unwrap().unwrap();
        assert_eq!(a.name, Some("A2".to_string()), "edited file must reparse");
        assert_eq!(
            b.name,
            Some("SentinelB".to_string()),
            "untouched file must stay served from cache"
        );
    }

    #[test]
    fn delete_cache_reproduces_byte_identical_results() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(
            &file,
            "---\nname: \"A\"\ntags:\n  - x\n  - y\n---\nbody text\n",
        );

        let mut warm = FreshnessCache::open_in(cache_base.path(), repo.path());
        let warm_result = warm.get_or_parse(&file).unwrap().unwrap();
        warm.save().unwrap();

        std::fs::remove_file(warm.path()).unwrap();

        let mut cold = FreshnessCache::open_in(cache_base.path(), repo.path());
        let cold_result = cold.get_or_parse(&file).unwrap().unwrap();

        assert_eq!(
            warm_result, cold_result,
            "a cold rebuild after deleting the cache must match the warm result exactly"
        );
    }

    #[test]
    fn corrupt_cache_file_is_a_miss_not_an_error() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(&file, "---\nname: \"A\"\n---\nbody\n");

        let probe = FreshnessCache::open_in(cache_base.path(), repo.path());
        let cache_path = probe.path().to_path_buf();
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(&cache_path, "{ this is not valid json").unwrap();

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        assert_eq!(
            cache.len(),
            0,
            "corrupt cache must load as empty, not error"
        );
        let parsed = cache.get_or_parse(&file).unwrap().unwrap();
        assert_eq!(parsed.name, Some("A".to_string()));
    }

    #[test]
    fn corrupt_cache_entry_shape_is_a_miss_not_an_error() {
        // Well-formed JSON, but not the CacheFile shape at all -- a
        // plausible corruption from a future/foreign navigator version.
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(&file, "---\nname: \"A\"\n---\nbody\n");

        let probe = FreshnessCache::open_in(cache_base.path(), repo.path());
        let cache_path = probe.path().to_path_buf();
        fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        fs::write(
            &cache_path,
            r#"{"entries": [{"totally": "unexpected shape"}]}"#,
        )
        .unwrap();

        let cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn malformed_frontmatter_is_reported_not_panicked_and_never_cached() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("bad.md");
        write_md(&file, "---\ntags: [unclosed\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        let result = cache.get_or_parse(&file).unwrap();
        assert!(result.is_err());
        assert_eq!(
            cache.len(),
            0,
            "a parse failure must not be cached as a hit"
        );
    }

    #[test]
    fn missing_file_is_an_io_error_not_a_panic() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        let result = cache.get_or_parse(&repo.path().join("does-not-exist.md"));
        assert!(result.is_err());
    }

    #[test]
    fn distinct_repo_roots_get_distinct_cache_files() {
        let cache_base = TempDir::new().unwrap();
        let repo_a = TempDir::new().unwrap();
        let repo_b = TempDir::new().unwrap();

        let cache_a = FreshnessCache::open_in(cache_base.path(), repo_a.path());
        let cache_b = FreshnessCache::open_in(cache_base.path(), repo_b.path());
        assert_ne!(cache_a.path(), cache_b.path());
    }

    #[test]
    fn non_utf8_body_bytes_do_not_panic_and_the_file_still_appears() {
        // Valid UTF-8 frontmatter header, but a body with a raw invalid
        // UTF-8 byte sequence. `from_utf8_lossy` must decode this without
        // panicking, and the file must still yield a successful parse
        // rather than being dropped as an I/O failure.
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        let mut bytes = b"---\nname: \"A\"\n---\nbody ".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE, 0xFD]);
        bytes.extend_from_slice(b" more\n");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, &bytes).unwrap();

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        let parsed = cache.get_or_parse(&file).unwrap().unwrap();
        assert_eq!(parsed.name, Some("A".to_string()));
    }

    #[test]
    fn same_mtime_second_but_different_size_is_detected_via_confirm_tier() {
        // Clock-skew / low mtime-resolution robustness: two writes that
        // land within the same whole mtime-second must still be told apart
        // because `size` is part of the fast-path key alongside mtime, not
        // mtime alone.
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(&file, "---\nname: \"A\"\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        cache.get_or_parse(&file).unwrap().unwrap();
        let stamp = fs::metadata(&file).unwrap().modified().unwrap();

        // Rewrite with different content/size, then pin mtime back to the
        // exact same instant the first write used -- a same-second (in
        // fact same-nanosecond) clock-skew scenario a coarse filesystem
        // timestamp could otherwise produce for two rapid writes.
        write_md(
            &file,
            "---\nname: \"A2\"\ntags:\n  - extra\n---\nlonger body\n",
        );
        let file_handle = fs::File::open(&file).unwrap();
        file_handle.set_modified(stamp).unwrap();

        let second = cache.get_or_parse(&file).unwrap().unwrap();
        assert_eq!(
            second.name,
            Some("A2".to_string()),
            "same mtime + different size must still be detected as changed, not served stale"
        );
    }

    #[test]
    fn known_limitation_same_mtime_same_size_different_content_is_undetected_by_fast_path() {
        // Documented fast-path limitation: mtime+size alone can't
        // distinguish two same-length rewrites landing at the identical
        // fingerprint. This is exactly why the confirm tier exists (a
        // caller that suspects this case can force a rehash by any other
        // trigger); the fast path alone is a known, accepted blind spot.
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(&file, "---\nname: \"AAA\"\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        cache.get_or_parse(&file).unwrap().unwrap();
        let stamp = fs::metadata(&file).unwrap().modified().unwrap();

        // Same byte length, different content, pinned to the identical
        // mtime -- the fast path's key (mtime, size) is unchanged, so it
        // reuses the stale cached parse rather than reparsing.
        write_md(&file, "---\nname: \"BBB\"\n---\nbody\n");
        let file_handle = fs::File::open(&file).unwrap();
        file_handle.set_modified(stamp).unwrap();

        let second = cache.get_or_parse(&file).unwrap().unwrap();
        assert_eq!(
            second.name,
            Some("AAA".to_string()),
            "documented limitation: same mtime+size masks a same-length content swap"
        );
    }

    #[test]
    fn save_is_byte_identical_across_repeated_saves_of_the_same_state() {
        let cache_base = TempDir::new().unwrap();
        let repo = TempDir::new().unwrap();
        let file = repo.path().join("a.md");
        write_md(&file, "---\nname: \"A\"\n---\nbody\n");

        let mut cache = FreshnessCache::open_in(cache_base.path(), repo.path());
        cache.get_or_parse(&file).unwrap().unwrap();
        cache.save().unwrap();
        let first = fs::read_to_string(cache.path()).unwrap();
        cache.save().unwrap();
        let second = fs::read_to_string(cache.path()).unwrap();
        assert_eq!(first, second);
    }
}
