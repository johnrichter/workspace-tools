//! The single source of truth for "never scanned/indexed" paths.
//!
//! Any future recursive content enumerator (search index, lint scanner,
//! freshness cache, symbol store) walks the reachable file set and must
//! decide, per path, whether to descend into or read it. That decision is
//! made exactly once, here — an enumerator calls [`is_skipped`] (or
//! [`is_skipped_dir_component`] while pruning a walk) instead of re-listing
//! any of these literals itself.
//!
//! # Enumerator-drift contract
//!
//! This crate has more than one enumerator of the same directory tree over
//! its lifetime: today the repo's `.gitignore`, and starting at the
//! search/lint milestone (M4) and the symbol store (v2/M7), navigator's own
//! recursive walks. Each enumerator that decides "scan or skip" for a path
//! MUST classify it the same way every other enumerator does — a path
//! skipped by one and scanned by another is a silent correctness bug, not a
//! visible error. Concretely:
//!
//! - Every recursive-walk enumerator this crate adds calls [`is_skipped`]
//!   (or the dir-component check, for pruning during the walk itself)
//!   instead of hardcoding its own exclusion list.
//! - The excluded set lives in exactly one place: [`SKIPPED_DIR_COMPONENTS`]
//!   and [`skipped_path_globs`] below. Extending the skip-set means editing
//!   those, never adding a parallel list in a new enumerator.
//! - `tests::plant_a_file` is the durable regression: it plants a real file
//!   under each excluded and non-excluded path and asserts `is_skipped`
//!   classifies it correctly. As M4/M7 land real enumerators, add each to
//!   that same assertion table so one planted file continues to prove every
//!   enumerator agrees.
//!
//! # The excluded set, and why
//!
//! - `.git` — version-control internals; not source content, and typically
//!   huge (full history) relative to the working tree.
//! - `node_modules`, `.venv` — installed third-party dependencies, not
//!   authored by this repo; scanning them wastes time and pollutes search
//!   results with vendored code the repo doesn't own.
//! - `reference-materials/code-repositories/**` — external clones this
//!   workspace pulls in for reference. They are protected, not authored:
//!   navigator must never scan, index, or lint content it doesn't own the
//!   source of. The glob is anchoring-robust (matches this subsequence at
//!   any depth, including under an absolute prefix) so that a misused input
//!   can never silently leave this tree scannable — see [`is_skipped`]'s
//!   input contract and [`skipped_path_globs`].
//!
//! # Relationship to `.gitignore`
//!
//! `.gitignore` and this skip-set both answer "what should be excluded from
//! this directory tree", but they are separate enumerators serving separate
//! purposes: `.gitignore` controls what Git tracks/commits, this module
//! controls what navigator scans/indexes at runtime. They are expected to
//! overlap (`.venv`, `node_modules`) but are not required to be identical —
//! `.gitignore` also excludes build output and OS/editor cruft navigator
//! has no reason to walk into anyway, and this skip-set additionally
//! excludes `reference-materials/code-repositories/**`, which IS tracked by
//! Git (it's real, committed content) but must never be treated as this
//! repo's own authored material. `tests::skipped_dir_components_are_also_gitignored`
//! is the cheap cross-check for the components that should overlap.

use std::path::{Component, Path};

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Directory-name components that, wherever they appear in a relative path,
/// mean "prune here — do not descend, do not scan anything under this
/// name." Checked by exact component match, not by glob, because these are
/// well-known fixed names rather than patterns.
pub const SKIPPED_DIR_COMPONENTS: &[&str] = &[".git", "node_modules", ".venv"];

/// Glob patterns for content skipped by location rather than by a fixed
/// directory name. See the module doc for why each pattern is here.
///
/// The patterns are **anchoring-robust**: the leading `**/` lets each match
/// its target subsequence wherever it sits in the path — at the reachable
/// root (`reference-materials/code-repositories/...`), nested deeper, or
/// under an absolute prefix (`/abs/.../reference-materials/...`). This is
/// deliberate. `GlobSet::is_match` anchors at the start of the string, so a
/// root-anchored pattern would silently NOT match an absolute path and the
/// protected tree would be scanned (silent under-exclusion — the dangerous
/// direction for a safety skip-set). The `**/` prefix trades that for
/// over-exclusion instead: a differently-rooted directory that happens to
/// end in the same subsequence is also skipped — the safe, refuse-to-scan
/// direction. See [`is_skipped`]'s input contract.
fn skipped_path_globs() -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    builder.add(Glob::new("**/reference-materials/code-repositories/**").unwrap());
    builder
        .build()
        .expect("skip-set globs are compile-time constants")
}

/// True if `rel_path` is a name an enumerator must never scan/index —
/// because it, or an ancestor of it, matches [`SKIPPED_DIR_COMPONENTS`] or
/// one of [`skipped_path_globs`].
///
/// # Input contract
///
/// Callers should pass a path **relative to the reachable-set root** (hence
/// the parameter name). That is the form a recursive content walk produces
/// once it strips the walk root, and it is the form in which the glob's
/// `reference-materials/code-repositories` location is exactly the root-level
/// protected tree — no more, no less.
///
/// Both branches are nonetheless anchoring-robust, so a caller that violates
/// the contract (passes an absolute path, or a path rooted deeper than the
/// reachable root) still gets a **safe** answer: the component check scans
/// every component regardless of where the path starts, and the glob is
/// `**/`-prefixed (see [`skipped_path_globs`]). A misused input can therefore
/// only ever *over*-exclude (skip something it needn't), never silently
/// *under*-exclude a protected tree — the property that matters for a
/// skip-set guarding external clones.
///
/// `rel_path` need not exist on disk; this is a pure, syntactic check over
/// the path's components, so callers can also use it to decide whether to
/// even `stat` a candidate.
pub fn is_skipped(rel_path: &Path) -> bool {
    if rel_path
        .components()
        .any(|component| is_skipped_dir_component(&component))
    {
        return true;
    }
    skipped_path_globs().is_match(rel_path)
}

/// True if a single path component is one of [`SKIPPED_DIR_COMPONENTS`] —
/// the check a recursive walk uses to prune a directory before descending
/// into it, without needing the rest of the path.
pub fn is_skipped_dir_component(component: &Component<'_>) -> bool {
    match component {
        Component::Normal(name) => SKIPPED_DIR_COMPONENTS
            .iter()
            .any(|skipped| name.to_str() == Some(*skipped)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// The plant-a-file drift regression: for every (path, expected)
    /// pair, write a real file at that relative path under a fresh temp
    /// root and assert `is_skipped` agrees with `expected`.
    ///
    /// This is the ONE table every enumerator must be checked against.
    /// `src/mdwalk.rs`'s recursive `.md` walk (M4's first real enumerator)
    /// is checked against this same planted tree below rather than a
    /// second, separate table — a future M7 enumerator (symbol store)
    /// should extend this test the same way. See the enumerator-drift rule
    /// this guards: `.claude/rules/shared-directory-enumerator-drift.md`.
    #[test]
    fn plant_a_file() {
        // Every skip-set case is planted as a `.md` file (rather than an
        // arbitrary extension) so the same table also exercises the
        // mdwalk cross-check below; `src/skipset.rs` stays non-`.md` on
        // purpose, to prove mdwalk excludes it by extension alone, with no
        // help from the skip-set.
        let cases: &[(&str, bool)] = &[
            (".git/x.md", true),
            ("node_modules/x.md", true),
            (".venv/x.md", true),
            (
                "reference-materials/code-repositories/some-clone/x.md",
                true,
            ),
            ("docs/x.md", false),
            ("README.md", false),
            ("src/skipset.rs", false),
        ];

        let root = TempDir::new().unwrap();
        for (rel, _) in cases {
            let path = root.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "planted").unwrap();
        }

        for (rel, expected) in cases {
            assert_eq!(
                is_skipped(&PathBuf::from(rel)),
                *expected,
                "is_skipped({rel:?}) should be {expected}"
            );
        }

        let found = crate::mdwalk::enumerate_markdown_files(&[root.path().to_path_buf()]);
        let found_rel: std::collections::HashSet<PathBuf> = found
            .iter()
            .map(|p| p.strip_prefix(root.path()).unwrap().to_path_buf())
            .collect();
        for (rel, skipped) in cases {
            // Case-sensitive `.md`, matching mdwalk's own extension check.
            let is_md = Path::new(rel).extension().and_then(|e| e.to_str()) == Some("md");
            let mdwalk_should_find_it = is_md && !skipped;
            assert_eq!(
                found_rel.contains(&PathBuf::from(rel)),
                mdwalk_should_find_it,
                "mdwalk::enumerate_markdown_files disagreed with is_skipped for {rel:?}"
            );
        }
    }

    /// Nested excluded directories are skipped via an ancestor component,
    /// not just a direct child — a walk must prune at the first excluded
    /// ancestor, not re-check every descendant individually.
    #[test]
    fn nested_paths_under_an_excluded_component_are_skipped() {
        assert!(is_skipped(Path::new("a/b/.git/objects/pack/x.pack")));
        assert!(is_skipped(Path::new("a/node_modules/pkg/index.js")));
        assert!(is_skipped(Path::new(".venv/lib/site-packages/x.py")));
    }

    /// A component that merely contains an excluded name as a substring
    /// (rather than matching it exactly) must NOT be skipped — the check is
    /// exact-component, not substring, matching.
    #[test]
    fn similar_but_distinct_component_names_are_not_skipped() {
        assert!(!is_skipped(Path::new("not-node_modules/x")));
        assert!(!is_skipped(Path::new("node_modules_backup/x")));
        assert!(!is_skipped(Path::new("dot-venv/x")));
    }

    /// `reference-materials/code-repositories/**` only excludes content
    /// under that exact prefix — a similarly-named sibling directory must
    /// stay scannable.
    #[test]
    fn reference_materials_glob_is_scoped_to_code_repositories() {
        assert!(!is_skipped(Path::new("reference-materials/dev/spec.md")));
        assert!(!is_skipped(Path::new("reference-materials/psa/notes.md")));
    }

    /// The path-glob branch alone (no excluded dir component in the path)
    /// still triggers the skip — proves the glob check isn't dead code
    /// shadowed by the component check.
    #[test]
    fn code_repositories_root_itself_is_skipped_by_glob_alone() {
        let path = Path::new("reference-materials/code-repositories/clone/README.md");
        assert!(!path.components().any(|c| is_skipped_dir_component(&c)));
        assert!(is_skipped(path));
    }

    /// Structural guard for the single-source invariant: the crate's only
    /// exclusion literals for `.git`/`node_modules`/`.venv` live in
    /// [`SKIPPED_DIR_COMPONENTS`], and the only skip-set glob lives in
    /// [`skipped_path_globs`]. This test can't grep other files at runtime,
    /// so it instead pins the exact excluded set here — if a future
    /// enumerator needs a new exclusion, growing this constant (and this
    /// assertion) is the only correct way to add it, never a second list
    /// elsewhere in the crate.
    #[test]
    fn skip_set_is_exactly_the_documented_literals() {
        assert_eq!(SKIPPED_DIR_COMPONENTS, &[".git", "node_modules", ".venv"]);
        assert!(skipped_path_globs().is_match("reference-materials/code-repositories/x"));
    }

    /// Cheap cross-check against the repo's `.gitignore`: every fixed
    /// directory-name component this module excludes at runtime is also
    /// one Git itself never tracks into. This does not assert full parity
    /// (see the module doc: the two enumerators serve different purposes
    /// and are allowed to diverge elsewhere) — only that the components
    /// most likely to silently drift (build/dependency directories) stay
    /// aligned.
    /// An excluded dir component not at the root of the path (but also not
    /// the deep multi-level case `nested_paths_under_an_excluded_component_are_skipped`
    /// already covers) — one level in, mid-path, still prunes.
    #[test]
    fn excluded_component_one_level_deep_is_skipped() {
        assert!(is_skipped(Path::new("src/vendor/.git/config")));
    }

    /// The `reference-materials/code-repositories/**` glob matches a file
    /// directly at that prefix (one path segment past the prefix itself),
    /// not only content several directories deeper — `**` must match zero
    /// or more segments, not one-or-more.
    #[test]
    fn code_repositories_glob_matches_one_level_as_well_as_deep() {
        assert!(is_skipped(Path::new(
            "reference-materials/code-repositories/x.md"
        )));
    }

    /// `is_skipped`/`is_skipped_dir_component` compare component text
    /// exactly, so a differently-cased name is a distinct component and is
    /// NOT skipped. This is a documented assumption (SC1's target OSes —
    /// macOS/Linux — commonly run case-sensitive filesystems for repo
    /// working trees), not an oversight: callers on a case-insensitive
    /// filesystem must not rely on this function alone for security-grade
    /// exclusion.
    #[test]
    fn differently_cased_component_is_not_skipped() {
        assert!(!is_skipped(Path::new(".GIT/config")));
        assert!(!is_skipped(Path::new("Node_Modules/x")));
    }

    /// Both branches of `is_skipped` are anchoring-robust, so a contract
    /// violation (an absolute path, or one rooted deeper than the reachable
    /// root) can only ever over-exclude, never silently under-exclude a
    /// protected tree. The component branch already scanned every component
    /// regardless of anchoring; the glob branch is `**/`-prefixed so it too
    /// matches its target subsequence under any leading prefix. An absolute
    /// path into a `reference-materials/code-repositories` clone — the exact
    /// safety case this skip-set guards — is therefore still skipped, and a
    /// genuinely unprotected absolute path is still scannable.
    #[test]
    fn absolute_paths_are_still_skipped_by_both_branches() {
        assert!(is_skipped(Path::new("/abs/project/.venv/lib/x.py")));
        assert!(is_skipped(Path::new(
            "/abs/reference-materials/code-repositories/clone/x.md"
        )));
        assert!(is_skipped(Path::new(
            "/abs/deeper/nest/reference-materials/code-repositories/x"
        )));
        assert!(!is_skipped(Path::new("/abs/project/src/main.rs")));
    }

    /// Windows path separators (`\`) are out of scope: SC1's target OSes
    /// are macOS/Linux (see module doc's OS assumptions upstream in this
    /// crate), and `std::path::Path` component-splitting on those platforms
    /// never treats `\` as a separator — a `\`-joined string arrives as one
    /// opaque `Normal` component, not a path to prune. No behavior to test;
    /// noted for anyone porting this crate to a Windows target.
    #[test]
    fn windows_separators_are_out_of_scope_for_target_oses() {
        assert!(!is_skipped(Path::new(r".venv\lib\x.py")));
    }

    /// Drift guard: if a future edit drops an entry from
    /// [`SKIPPED_DIR_COMPONENTS`] (e.g. narrows it to just `.git`) or
    /// removes the `reference-materials/code-repositories/**` glob, this
    /// test's exact-length/content assertions fail immediately — same as
    /// `skip_set_is_exactly_the_documented_literals`, restated against the
    /// live behavior (`is_skipped`) rather than the constant alone, so a
    /// change that keeps the constant's shape but breaks the predicate
    /// wiring is also caught.
    #[test]
    fn dropping_any_skip_set_entry_would_fail_this_test() {
        assert_eq!(SKIPPED_DIR_COMPONENTS.len(), 3, "add/remove entries here AND in the plant_a_file/is_skipped assertions below, never independently");
        for (entry, sample) in [
            (".git", "a/.git/x"),
            ("node_modules", "a/node_modules/x"),
            (".venv", "a/.venv/x"),
        ] {
            assert!(
                SKIPPED_DIR_COMPONENTS.contains(&entry),
                "{entry} missing from SKIPPED_DIR_COMPONENTS"
            );
            assert!(
                is_skipped(Path::new(sample)),
                "is_skipped({sample:?}) must be true while {entry} is in the skip-set"
            );
        }
        assert!(is_skipped(Path::new(
            "reference-materials/code-repositories/x"
        )));
    }
}
