//! `navigator lint`: a deterministic, schema-driven scan of the reachable
//! `.md` set, emitting a [`FrontmatterEntry`]-shaped verdict per file plus a
//! coverage rollup. All the actual rule logic (required fields, tag-namespace
//! cascade, description caps, exempt paths) lives in `frontmatter::validate`
//! -- this module's job is wiring: validate each file against the resolved
//! [`Profile`] (see `crate::profile_resolve` for sentinel -> profile
//! resolution), compute each file's `rel_path` (the input `validate` derives
//! `file_class`/exemption from), and fold every outcome into one rollup.
//!
//! # `rel_path` contract
//! `validate` derives `file_class` (and exempt-path membership) from the
//! `rel_path` string passed to it -- a repo-root-relative, forward-slash
//! POSIX path. [`repo_relative_posix`] strips `repo_root` (the launched-in
//! `cwd`, the same root [`sentinel::load`] reads from) and rejoins path
//! components with `/`, so classification is stable across platforms. A
//! scanned file outside `repo_root` (reachable only via an attached `--dir`
//! or settings-granted directory) can't be stripped; it falls back to its
//! full path with the same forward-slash join, which still classifies
//! correctly against any glob that doesn't assume a repo-root-relative
//! shape (most exempt/file-class globs are anchored on a leaf pattern like
//! `**/SKILL.md`, so this is a reasonable approximation, not a correctness
//! guarantee for every possible glob).
//!
//! # Missing vs. malformed frontmatter
//! A file whose `parse` result is `Err` (malformed YAML, unclosed
//! delimiter, ...) or `Ok` with empty `raw_fields` (no frontmatter block, or
//! an empty one) never reaches `validate` -- both cases fold as
//! [`ScanOutcome::Missing`] into the rollup's `missing_frontmatter` count,
//! per `frontmatter::validate`'s own documented split. This module never
//! synthesizes a violation code for either case.
//!
//! # Exit code
//! `navigator lint` is a report, not an action -- but a gate hook needs a
//! machine-checkable pass/fail signal, so this command's own convention
//! (distinct from clap's usage-error code) is: [`exit_code::LINT_FINDINGS`]
//! when the rollup shows any invalid or missing-frontmatter file,
//! [`exit_code::SUCCESS`] otherwise (including when the reachable set is
//! empty). See `main.rs::run_lint`.

use std::path::{Path, PathBuf};

use frontmatter::validate::fold;
use frontmatter::{validate, CoverageRollup, ParsedFrontmatter, Profile, RawFields, ScanOutcome};
use serde::Serialize;

use crate::cli::LintArgs;
use crate::profile_resolve::ResolvedExempt;
use crate::scan::ScannedFile;

/// A [`frontmatter::Violation`], reshaped for serialization -- `Violation`
/// itself derives no `Serialize` (it lives in a crate that doesn't need
/// JSON output), so this mirrors its three fields verbatim.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LintViolation {
    pub code: String,
    pub field: String,
    pub message: String,
}

/// One scanned file's lint verdict -- the shared payload for both JSON and
/// terse output.
///
/// `file_class`/`violations` are only populated for a file that reached
/// `validate` at all; `missing_frontmatter` distinguishes "no usable
/// frontmatter to validate" (no class, no violations, counted in the
/// rollup's own bucket) from "validated and found invalid" (a real
/// violation list) -- both are `is_valid: false`, but a caller wanting the
/// literal `FrontmatterEntry` split needs this extra flag rather than
/// conflating the two under one boolean.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LintFileResult {
    pub path: String,
    pub file_class: Option<String>,
    pub is_valid: bool,
    pub missing_frontmatter: bool,
    pub violations: Vec<LintViolation>,
}

/// The scan-wide coverage rollup, reshaped for serialization -- mirrors
/// [`CoverageRollup`]'s four fields verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct LintRollup {
    pub scanned: u64,
    pub valid: u64,
    pub invalid: u64,
    pub missing_frontmatter: u64,
}

impl From<CoverageRollup> for LintRollup {
    fn from(rollup: CoverageRollup) -> Self {
        Self {
            scanned: rollup.scanned,
            valid: rollup.valid,
            invalid: rollup.invalid,
            missing_frontmatter: rollup.missing_frontmatter,
        }
    }
}

/// The full `lint` result -- every scoped file's verdict, path-sorted, plus
/// the rollup folded across them. JSON and terse rendering both read this
/// one value (see [`render_terse`]), so the two modes can never report a
/// different file set or order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LintResult {
    pub files: Vec<LintFileResult>,
    pub rollup: LintRollup,
}

/// `args.scope`, resolved to an absolute path against `repo_root` (`cwd`) if
/// relative, or `None` when no scope was given (lint the whole reachable
/// set).
///
/// `pub(crate)`: `crate::fix` reuses this exact resolution so `fix`'s scope
/// semantics (single file or directory, relative-to-`cwd`) never diverge
/// from `lint`'s.
pub(crate) fn resolve_scope(scope: Option<&Path>, repo_root: &Path) -> Option<PathBuf> {
    scope.map(|scope| {
        let joined = repo_root.join(scope);
        std::path::absolute(&joined).unwrap_or(joined)
    })
}

/// True iff `path` is `scope` itself or lives under it -- `scope` may name
/// either a single file or a directory; `None` (no `--scope`) always passes.
///
/// `pub(crate)`: see [`resolve_scope`].
pub(crate) fn passes_scope(path: &Path, scope: Option<&Path>) -> bool {
    scope.is_none_or(|scope| path == scope || path.starts_with(scope))
}

/// `path`'s POSIX, forward-slash-joined path relative to `repo_root` -- the
/// `rel_path` `validate` derives `file_class`/exemption from. See this
/// module's doc comment for the out-of-`repo_root` fallback.
///
/// `pub(crate)`: `crate::conformance` (shared by `search`/`find`) reuses
/// this exact derivation so a file's `rel_path` -- and therefore its
/// conformance verdict -- never diverges between `lint` and its callers.
pub(crate) fn repo_relative_posix(path: &Path, repo_root: &Path) -> String {
    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Runs `args`' lint over `scanned` (the pre-computed corpus from
/// [`crate::scan::scan`]), validating every in-scope file against `profile`
/// and folding the coverage rollup, per this module's doc comment.
#[must_use]
pub fn run(
    scanned: &[ScannedFile],
    args: &LintArgs,
    repo_root: &Path,
    profile: &Profile,
) -> LintResult {
    let scope = resolve_scope(args.scope.as_deref(), repo_root);

    let mut in_scope: Vec<&ScannedFile> = scanned
        .iter()
        .filter(|file| passes_scope(&file.path, scope.as_deref()))
        .collect();
    in_scope.sort_by(|a, b| a.path.cmp(&b.path));

    let mut rollup = CoverageRollup::default();
    let files: Vec<LintFileResult> = in_scope
        .into_iter()
        .map(|file| {
            let path = file.path.to_string_lossy().into_owned();
            let Ok(parsed) = &file.parsed else {
                fold(&mut rollup, ScanOutcome::Missing);
                return LintFileResult {
                    path,
                    file_class: None,
                    is_valid: false,
                    missing_frontmatter: true,
                    violations: Vec::new(),
                };
            };
            if parsed.raw_fields.is_empty() {
                fold(&mut rollup, ScanOutcome::Missing);
                return LintFileResult {
                    path,
                    file_class: None,
                    is_valid: false,
                    missing_frontmatter: true,
                    violations: Vec::new(),
                };
            }

            let rel_path = repo_relative_posix(&file.path, repo_root);
            let entry = validate(parsed, &rel_path, profile);
            fold(&mut rollup, ScanOutcome::Entry(&entry));
            LintFileResult {
                path,
                file_class: Some(entry.file_class.clone()),
                is_valid: entry.is_valid,
                missing_frontmatter: false,
                violations: entry
                    .violations
                    .iter()
                    .map(|v| LintViolation {
                        code: v.code.clone(),
                        field: v.field.clone(),
                        message: v.message.clone(),
                    })
                    .collect(),
            }
        })
        .collect();

    LintResult {
        files,
        rollup: rollup.into(),
    }
}

/// Reclassifies every `missing_frontmatter` file in `result` that `exempt`
/// covers as valid, adjusting `result.rollup` to match.
///
/// [`run`]'s Missing short circuit (see this module's doc comment) never
/// reaches [`validate`], the only place that consults a pack's `exempt`
/// vocabulary -- so a deliberately frontmatter-free file (a plain README, a
/// test fixture) that a pack exempts stays exempt from field/tag checks but,
/// without this pass, still fails the gate on `missing_frontmatter` alone.
/// `exempt` is [`crate::profile_resolve`]'s independent read of the same
/// pack-declared vocabulary, taken over `rel_path` the same way [`run`]
/// derives it.
///
/// Once a file is confirmed exempt, `validate` is called on an empty
/// placeholder purely to get the same `file_class` an exempt file with real
/// frontmatter would report -- safe here specifically because exemption is
/// already decided independently; `validate`'s own `exempt_gate` runs first
/// and short-circuits before any field/tag check the placeholder's empty
/// content would otherwise fail.
pub fn reclassify_exempt_missing(
    result: &mut LintResult,
    exempt: &ResolvedExempt,
    profile: &Profile,
    repo_root: &Path,
) {
    for file in &mut result.files {
        if !file.missing_frontmatter {
            continue;
        }
        let rel_path = repo_relative_posix(Path::new(&file.path), repo_root);
        if !exempt.is_exempt(&rel_path) {
            continue;
        }

        let placeholder = ParsedFrontmatter {
            tags: Vec::new(),
            name: None,
            id: None,
            description: None,
            body_text: String::new(),
            raw_fields: RawFields::from_ordered_pairs(Vec::new()),
        };
        let entry = validate(&placeholder, &rel_path, profile);

        file.is_valid = true;
        file.missing_frontmatter = false;
        file.file_class = Some(entry.file_class);
        result.rollup.missing_frontmatter -= 1;
        result.rollup.valid += 1;
    }
}

/// Renders `result` as one line per file (`path: status (N violation(s))`),
/// each followed by one indented `code: message` line per violation on that
/// file, then a rollup summary line. Reads the exact `Vec` [`run`] returns,
/// in its exact order -- the mechanism that keeps terse output's file
/// set/order identical to the JSON encoding of the same [`LintResult`].
// Retained as a test-only shape/determinism helper: the shipped output is
// the clikit `ResultRecord` on stdout, never a terse human summary.
#[cfg(test)]
fn render_terse(result: &LintResult) -> String {
    let mut lines: Vec<String> = Vec::new();
    for file in &result.files {
        let status = if file.missing_frontmatter {
            "missing"
        } else if file.is_valid {
            "valid"
        } else {
            "invalid"
        };
        lines.push(format!(
            "{}: {status} ({} violation{})",
            file.path,
            file.violations.len(),
            if file.violations.len() == 1 { "" } else { "s" }
        ));
        for violation in &file.violations {
            lines.push(format!("  {}: {}", violation.code, violation.message));
        }
    }
    let rollup = &result.rollup;
    lines.push(format!(
        "scanned={} valid={} invalid={} missing_frontmatter={}",
        rollup.scanned, rollup.valid, rollup.invalid, rollup.missing_frontmatter
    ));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FreshnessCache;
    use std::fs;
    use tempfile::TempDir;

    fn write_md(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }

    fn args() -> LintArgs {
        LintArgs {
            scope: None,
            dir: Vec::new(),
        }
    }

    fn scan_fixture(root: &Path) -> Vec<ScannedFile> {
        let cache_base = TempDir::new().unwrap();
        let mut cache = FreshnessCache::open_in(cache_base.path(), root);
        crate::scan::scan(&[root.to_path_buf()], &mut cache)
    }

    fn conformant_frontmatter() -> &'static str {
        "---\n\
name: \"Doc\"\n\
description: \"A conformant test document.\"\n\
id: \"knowledge-base:test:doc\"\n\
tags:\n\
  - type:knowledge\n\
  - topic:testing\n\
  - status:complete\n\
  - privacy:example\n\
  - owner:example\n\
links: []\n\
updated: 2026-07-11T00:00:00Z\n\
---\n\
body\n"
    }

    // -- Per-file verdicts + rollup ---------------------------------------

    #[test]
    fn valid_file_is_valid_with_no_violations_and_counted_in_rollup() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", conformant_frontmatter());
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].is_valid, "{:?}", result.files[0].violations);
        assert!(!result.files[0].missing_frontmatter);
        assert_eq!(result.files[0].file_class, Some("context".to_string()));
        assert_eq!(result.rollup.scanned, 1);
        assert_eq!(result.rollup.valid, 1);
        assert_eq!(result.rollup.invalid, 0);
        assert_eq!(result.rollup.missing_frontmatter, 0);
    }

    #[test]
    fn invalid_file_reports_the_right_codes_and_fields() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "---\nname: \"x\"\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        let file = &result.files[0];
        assert!(!file.is_valid);
        assert!(!file.missing_frontmatter);
        let missing_fields: Vec<&str> = file
            .violations
            .iter()
            .filter(|v| v.code == "MISSING_REQUIRED_FIELD")
            .map(|v| v.field.as_str())
            .collect();
        assert_eq!(
            missing_fields,
            vec!["description", "id", "tags", "links", "updated"]
        );
        assert_eq!(result.rollup.invalid, 1);
        assert_eq!(result.rollup.valid, 0);
    }

    #[test]
    fn malformed_frontmatter_is_missing_not_a_synthesized_violation() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "bad.md", "---\ntags: [unclosed\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        let file = &result.files[0];
        assert!(file.missing_frontmatter);
        assert!(!file.is_valid);
        assert!(file.violations.is_empty(), "no synthesized violation code");
        assert_eq!(file.file_class, None);
        assert_eq!(result.rollup.missing_frontmatter, 1);
        assert_eq!(result.rollup.invalid, 0);
    }

    #[test]
    fn absent_frontmatter_block_is_missing_not_a_synthesized_violation() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "no-fm.md",
            "# just a heading\nno frontmatter\n",
        );
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].missing_frontmatter);
        assert!(result.files[0].violations.is_empty());
        assert_eq!(result.rollup.missing_frontmatter, 1);
    }

    // -- Skip-set + scope ---------------------------------------------------

    #[test]
    fn skip_set_excludes_files_under_a_skipped_directory() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "keep.md", conformant_frontmatter());
        write_md(root.path(), ".git/skip.md", conformant_frontmatter());
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].path.ends_with("keep.md"));
    }

    #[test]
    fn scope_restricts_to_files_under_the_given_directory() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "in-scope/a.md", conformant_frontmatter());
        write_md(root.path(), "out-of-scope/b.md", conformant_frontmatter());
        let scanned = scan_fixture(root.path());

        let mut a = args();
        a.scope = Some(PathBuf::from("in-scope"));
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &a, root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].path.contains("in-scope"));
    }

    #[test]
    fn scope_restricts_to_a_single_file() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "a.md", conformant_frontmatter());
        write_md(root.path(), "b.md", conformant_frontmatter());
        let scanned = scan_fixture(root.path());

        let mut a = args();
        a.scope = Some(PathBuf::from("a.md"));
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &a, root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].path.ends_with("a.md"));
    }

    // -- file_class classification via rel_path ----------------------------

    #[test]
    fn agent_file_under_dot_claude_agents_gets_agent_class() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), ".claude/agents/x.md", conformant_frontmatter());
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].file_class, Some("agent".to_string()));
    }

    #[test]
    fn skill_md_gets_skill_class() {
        let root = TempDir::new().unwrap();
        let input = concat!(
            "---\n",
            "name: \"My Skill\"\n",
            "description: \"top-level description within the skill cap.\"\n",
            "workspace:\n",
            "  id: \"skill:workspace:my-skill\"\n",
            "  links: []\n",
            "  updated: 2026-07-11T00:00:00Z\n",
            "  tags:\n",
            "    - type:skill\n",
            "    - topic:testing\n",
            "    - status:complete\n",
            "    - privacy:example\n",
            "    - owner:example\n",
            "---\n",
            "body\n"
        );
        write_md(root.path(), ".claude/skills/my-skill/SKILL.md", input);
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].file_class, Some("skill".to_string()));
        assert!(result.files[0].is_valid, "{:?}", result.files[0].violations);
    }

    #[test]
    fn other_file_gets_context_class() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "knowledge-base/test/doc.md",
            conformant_frontmatter(),
        );
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].file_class, Some("context".to_string()));
    }

    // -- Exempt files ---------------------------------------------------

    #[test]
    fn exempt_file_is_valid_regardless_of_content() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "plan.md", "---\nname: \"x\"\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].is_valid);
        assert!(result.files[0].violations.is_empty());
        assert_eq!(result.rollup.valid, 1);
    }

    // -- JSON/terse parity, determinism ------------------------------------

    #[test]
    fn json_and_terse_report_the_same_file_set_and_order() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "b.md", conformant_frontmatter());
        write_md(root.path(), "a.md", "---\nname: \"x\"\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let terse = render_terse(&result);
        let terse_paths: Vec<&str> = terse
            .lines()
            .filter(|l| !l.starts_with("scanned=") && !l.starts_with("  "))
            .map(|l| l.split(':').next().unwrap())
            .collect();
        let json_paths: Vec<&str> = result.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(terse_paths, json_paths);
        assert_eq!(json_paths, vec![json_paths[0], json_paths[1]]);
        assert!(json_paths[0].ends_with("a.md"));
        assert!(json_paths[1].ends_with("b.md"));
    }

    #[test]
    fn repeated_runs_are_byte_identical() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "a.md", conformant_frontmatter());
        write_md(root.path(), "b.md", "---\nname: \"x\"\n---\nbody\n");
        write_md(root.path(), "c.md", "---\ntags: [unclosed\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let first = run(&scanned, &args(), root.path(), &profile);
        let second = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(first, second);
        assert_eq!(render_terse(&first), render_terse(&second));
    }

    // -- Hardening: per-file verdict correctness, every invalid kind -------
    // Each fixture below is lifted verbatim from `frontmatter::validate`'s
    // own unit tests (the source of truth for which code a given input
    // fires); these tests confirm `lint::run` is a faithful front end over
    // that verdict, not a second implementation of the rule.

    #[test]
    fn singleton_duplicate_tag_fires_multiple_single_value_tags() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "doc.md",
            "---\nname: \"x\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags:\n  - type:a\n  - type:b\n  - status:complete\n  - privacy:example\n  - owner:example\n  - topic:t\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let file = &result.files[0];
        assert!(!file.is_valid);
        assert!(!file.missing_frontmatter);
        let violation = file
            .violations
            .iter()
            .find(|v| v.code == "MULTIPLE_SINGLE_VALUE_TAGS")
            .expect("expected MULTIPLE_SINGLE_VALUE_TAGS");
        assert_eq!(violation.field, "type");
    }

    #[test]
    fn missing_required_tag_namespace_fires_missing_required_tag() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "doc.md",
            "---\nname: \"x\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags:\n  - status:complete\n  - privacy:example\n  - owner:example\n  - topic:t\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let file = &result.files[0];
        assert!(file
            .violations
            .iter()
            .any(|v| v.code == "MISSING_REQUIRED_TAG" && v.field == "type"));
    }

    #[test]
    fn orphan_namespace_tag_fires_when_parent_absent() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "doc.md",
            "---\nname: \"x\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags:\n  - type:knowledge\n  - status:complete\n  - privacy:example\n  - owner:example\n  - topic:t\n  - feature:trace-explorer\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let file = &result.files[0];
        let orphans: Vec<&str> = file
            .violations
            .iter()
            .filter(|v| v.code == "ORPHAN_NAMESPACE_TAG")
            .map(|v| v.field.as_str())
            .collect();
        assert_eq!(orphans, vec!["feature", "feature"]);
    }

    #[test]
    fn report_only_tag_misused_fires_on_a_non_report_file() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "doc.md",
            "---\nname: \"x\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags:\n  - type:knowledge\n  - status:complete\n  - privacy:example\n  - owner:example\n  - topic:t\n  - source:slack\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let file = &result.files[0];
        assert!(file
            .violations
            .iter()
            .any(|v| v.code == "REPORT_ONLY_TAG_MISUSED" && v.field == "source"));
    }

    #[test]
    fn bad_period_format_fires_invalid_period_format_on_a_report_file() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "report.md",
            "---\nname: \"x\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags:\n  - type:report\n  - status:complete\n  - privacy:example\n  - owner:example\n  - topic:t\n  - source:slack\n  - period:not-a-range\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let file = &result.files[0];
        let violation = file
            .violations
            .iter()
            .find(|v| v.code == "INVALID_PERIOD_FORMAT")
            .expect("expected INVALID_PERIOD_FORMAT");
        assert_eq!(violation.field, "period");
    }

    #[test]
    fn tags_not_a_list_fires_when_tags_is_a_scalar() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "doc.md",
            "---\nname: \"x\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags: not-a-list\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let file = &result.files[0];
        assert!(file
            .violations
            .iter()
            .any(|v| v.code == "TAGS_NOT_A_LIST" && v.field == "tags"));
    }

    #[test]
    fn description_over_cap_fires_for_the_file_classs_own_cap() {
        // 350 is the "context" cap (the class this fixture's path resolves
        // to); the description below is 351 chars, one over.
        let root = TempDir::new().unwrap();
        let long_description = "d".repeat(351);
        write_md(
            root.path(),
            "doc.md",
            &format!(
                "---\nname: \"x\"\ndescription: \"{long_description}\"\nid: \"a:b:c\"\ntags:\n  - type:knowledge\n  - status:complete\n  - privacy:example\n  - owner:example\n  - topic:t\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n"
            ),
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let file = &result.files[0];
        assert_eq!(file.file_class, Some("context".to_string()));
        assert!(file
            .violations
            .iter()
            .any(|v| v.code == "DESCRIPTION_OVER_CAP" && v.field == "description"));
    }

    #[test]
    fn default_terse_output_prints_the_violating_codes_and_messages_not_just_the_count() {
        // A file over its class's description cap must be diagnosable from
        // ONE run of default (non-JSON) output -- the code and message,
        // not merely a violation count, must appear next to the file line.
        let root = TempDir::new().unwrap();
        let long_description = "d".repeat(351);
        write_md(
            root.path(),
            "over_cap.md",
            &format!(
                "---\nname: \"x\"\ndescription: \"{long_description}\"\nid: \"a:b:c\"\ntags:\n  - type:knowledge\n  - status:complete\n  - privacy:example\n  - owner:example\n  - topic:t\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n"
            ),
        );
        write_md(root.path(), "clean.md", conformant_frontmatter());
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);

        let over_cap_file = result
            .files
            .iter()
            .find(|f| f.path.ends_with("over_cap.md"))
            .unwrap();
        let violation = over_cap_file
            .violations
            .iter()
            .find(|v| v.code == "DESCRIPTION_OVER_CAP")
            .expect("fixture must fire DESCRIPTION_OVER_CAP");

        let terse = render_terse(&result);
        let expected_line = format!("  {}: {}", violation.code, violation.message);
        assert!(
            terse.contains(&expected_line),
            "default output must contain the violation's code+message line {expected_line:?}; got:\n{terse}"
        );

        // The code+message line must appear directly under its OWN file's
        // summary line, not merely somewhere in the output.
        let lines: Vec<&str> = terse.lines().collect();
        let file_line_idx = lines
            .iter()
            .position(|l| l.starts_with(&over_cap_file.path) && !l.starts_with("  "))
            .unwrap();
        assert_eq!(lines[file_line_idx + 1], expected_line);

        // A clean file contributes no indented violation lines at all --
        // the per-violation line is additive, never emitted for valid files.
        let clean_file = result
            .files
            .iter()
            .find(|f| f.path.ends_with("clean.md"))
            .unwrap();
        assert!(clean_file.violations.is_empty());
        let clean_line_idx = lines
            .iter()
            .position(|l| l.starts_with(&clean_file.path) && !l.starts_with("  "))
            .unwrap();
        assert!(
            clean_line_idx + 1 == lines.len() || !lines[clean_line_idx + 1].starts_with("  "),
            "a clean file must not be followed by an indented violation line"
        );
    }

    #[test]
    fn every_violation_on_a_multi_violation_file_gets_its_own_terse_line() {
        // A file firing more than one violation (missing frontmatter fields
        // AND an over-cap description is not directly constructible here,
        // so use two independently-triggerable MISSING_REQUIRED_FIELD-style
        // violations via a minimal frontmatter) must surface every one of
        // them, not just the first, under default output.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "multi.md",
            "---\nname: \"x\"\ntags: not-a-list\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        let file = &result.files[0];
        assert!(
            file.violations.len() >= 2,
            "fixture must fire multiple violations to exercise this case: {:?}",
            file.violations
        );

        let terse = render_terse(&result);
        for v in &file.violations {
            let expected_line = format!("  {}: {}", v.code, v.message);
            assert!(
                terse.contains(&expected_line),
                "missing line for violation {v:?} in:\n{terse}"
            );
        }
    }

    // -- Hardening: rel_path / file_class edge cases -------------------------

    #[test]
    fn nested_agent_path_classifies_as_agent_matching_validate_not_the_python_original() {
        // `frontmatter::validate`'s declarative `file_class` glob for
        // `.claude/agents/*.md` matches nested paths too (`*` spans `/`,
        // documented residual divergence in `DIVERGENCES.md` D3 -- the
        // Python original's exact-depth-3 predicate would classify this as
        // `context` instead). This test pins `lint::run`'s rel_path wiring
        // to whatever `validate` ACTUALLY does today, not to the Python
        // original's stricter rule -- if `validate`'s glob is ever tightened
        // to match the Python original, this assertion is the one to flip.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            ".claude/agents/sub/x.md",
            conformant_frontmatter(),
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].file_class, Some("agent".to_string()));
    }

    #[test]
    fn file_outside_repo_root_falls_back_to_its_full_path_and_still_classifies() {
        let repo_root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write_md(outside.path(), "SKILL.md", conformant_frontmatter());
        let scanned = scan_fixture(outside.path());

        let mut a = args();
        a.dir = vec![outside.path().to_path_buf()];
        let profile = crate::test_support::default_profile_for_tests();
        // `repo_root` (repo_root.path()) shares no prefix with the scanned
        // file (outside.path()), so `repo_relative_posix`'s
        // `strip_prefix` fails and falls back to the full path -- this
        // still classifies correctly against the unanchored `**/SKILL.md`
        // rule.
        let result = run(&scanned, &a, repo_root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].file_class, Some("skill".to_string()));
    }

    // -- Hardening: exempt wins over an otherwise-invalid file --------------

    #[test]
    fn exempt_file_wins_even_when_content_would_fail_every_check() {
        // Non-empty frontmatter (so this reaches `validate`, not the
        // Missing short-circuit) that would fire TAGS_NOT_A_LIST plus every
        // MISSING_REQUIRED_FIELD if `plan.md` weren't exempt.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "plan.md",
            "---\nname: \"x\"\ntags: not-a-list\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);
        assert_eq!(result.files.len(), 1);
        let file = &result.files[0];
        assert!(!file.missing_frontmatter);
        assert!(
            file.is_valid,
            "exempt file must win over every check: {:?}",
            file.violations
        );
        assert!(file.violations.is_empty());
    }

    // -- Hardening: rollup sum invariant -------------------------------------

    #[test]
    fn rollup_sum_invariant_holds_across_a_mixed_corpus() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "valid.md", conformant_frontmatter());
        write_md(root.path(), "invalid.md", "---\nname: \"x\"\n---\nbody\n");
        write_md(
            root.path(),
            "malformed.md",
            "---\ntags: [unclosed\n---\nbody\n",
        );
        write_md(root.path(), "no-fm.md", "# heading only\n");
        write_md(root.path(), "plan.md", "---\nname: \"x\"\n---\nbody\n");
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args(), root.path(), &profile);

        let rollup = result.rollup;
        assert_eq!(
            rollup.valid + rollup.invalid + rollup.missing_frontmatter,
            rollup.scanned,
            "valid+invalid+missing must sum to scanned: {rollup:?}"
        );
        assert_eq!(rollup.scanned, result.files.len() as u64);
    }

    // -- reclassify_exempt_missing ------------------------------------------

    /// Writes a minimal committed pack at `<root>/pack.json` extending
    /// `core@2.0.0`, `exempt`ing exactly `filenames`/`path_globs`, and a
    /// `navigator.toml` sentinel loading it -- enough for
    /// [`crate::profile_resolve::resolve`] to produce a real
    /// [`crate::profile_resolve::ResolvedExempt`] to exercise
    /// [`reclassify_exempt_missing`] against (that struct has no public
    /// constructor other than going through `resolve`).
    fn write_sentinel_with_exempt_pack(root: &Path, filenames: &[&str], path_globs: &[&str]) {
        let filenames_json = filenames
            .iter()
            .map(|f| format!("{f:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let globs_json = path_globs
            .iter()
            .map(|g| format!("{g:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            root.join("pack.json"),
            format!(
                r#"{{
                    "kind": "extension-pack",
                    "version": "test-exempt@1",
                    "extends": "core@2.0.0",
                    "required_fields": [{{"field": "name", "authorship": "human_authored"}}],
                    "description_caps": {{"context": 500}},
                    "file_class": {{"default": "context", "rules": []}},
                    "namespaces": [{{"name": "type", "cardinality": "optional"}}],
                    "exempt": {{"filenames": [{filenames_json}], "dir_components": [], "path_globs": [{globs_json}]}}
                }}"#
            ),
        )
        .unwrap();
        fs::write(
            root.join("navigator.toml"),
            "sentinel_version = 2\nextensions = [\"pack.json\"]\n\n[schema]\nprofile = \"core@2.0.0\"\n",
        )
        .unwrap();
    }

    fn resolve_test_profile(root: &Path) -> crate::profile_resolve::Resolution {
        crate::profile_resolve::resolve(root, crate::profile_resolve::ResolveMode::Gate).unwrap()
    }

    #[test]
    fn exempt_covers_missing_frontmatter_flips_to_valid_and_updates_rollup() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "README.md", "no frontmatter block at all\n");
        write_md(root.path(), "other.md", "no frontmatter block at all\n");
        let scanned = scan_fixture(root.path());
        write_sentinel_with_exempt_pack(root.path(), &["README.md"], &[]);
        let resolution = resolve_test_profile(root.path());

        let mut result = run(&scanned, &args(), root.path(), &resolution.profile);
        // Non-vacuity: before reclassify runs, BOTH files sit in the
        // missing_frontmatter bucket -- proves the exempt-vs-not split
        // below is reclassify_exempt_missing's own effect, not
        // validate()'s (which the Missing short circuit never reaches).
        assert_eq!(result.rollup.missing_frontmatter, 2);
        assert_eq!(result.rollup.valid, 0);

        reclassify_exempt_missing(
            &mut result,
            &resolution.exempt,
            &resolution.profile,
            root.path(),
        );

        let readme = result
            .files
            .iter()
            .find(|f| f.path.ends_with("README.md"))
            .unwrap();
        assert!(
            readme.is_valid,
            "exempt missing-frontmatter file must flip to valid"
        );
        assert!(!readme.missing_frontmatter);
        assert!(readme.file_class.is_some());
        assert!(readme.violations.is_empty());

        let other = result
            .files
            .iter()
            .find(|f| f.path.ends_with("other.md"))
            .unwrap();
        assert!(
            !other.is_valid,
            "non-exempt missing-frontmatter file must NOT flip"
        );
        assert!(other.missing_frontmatter);

        assert_eq!(result.rollup.valid, 1);
        assert_eq!(result.rollup.missing_frontmatter, 1);
        assert_eq!(result.rollup.invalid, 0);
        assert_eq!(
            result.rollup.valid + result.rollup.invalid + result.rollup.missing_frontmatter,
            result.rollup.scanned
        );
    }

    #[test]
    fn non_exempt_missing_frontmatter_is_unaffected_by_reclassify() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "other.md", "no frontmatter block at all\n");
        let scanned = scan_fixture(root.path());
        // Exempt block covers a path this repo doesn't have -- nothing to flip.
        write_sentinel_with_exempt_pack(root.path(), &["README.md"], &[]);
        let resolution = resolve_test_profile(root.path());

        let mut result = run(&scanned, &args(), root.path(), &resolution.profile);
        let before = result.rollup;

        reclassify_exempt_missing(
            &mut result,
            &resolution.exempt,
            &resolution.profile,
            root.path(),
        );

        assert_eq!(result.rollup, before, "reclassify must be a no-op here");
        assert!(result.files[0].missing_frontmatter);
        assert!(!result.files[0].is_valid);
    }

    #[test]
    fn exempt_path_glob_with_real_but_rule_violating_frontmatter_is_valid_via_validate_own_exempt_gate_not_reclassify(
    ) {
        let root = TempDir::new().unwrap();
        // Real, parseable frontmatter missing the required `name` field --
        // would be `invalid` (not `missing`) if not exempt.
        write_md(
            root.path(),
            "testdata/fixture.md",
            "---\ndescription: \"x\"\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        write_sentinel_with_exempt_pack(root.path(), &[], &["testdata/*"]);
        let resolution = resolve_test_profile(root.path());

        let mut result = run(&scanned, &args(), root.path(), &resolution.profile);
        // Already valid before reclassify even runs: this is validate()'s
        // OWN exempt_gate (a well-formed frontmatter block always reaches
        // validate, never the Missing short circuit) -- reclassify_exempt_missing
        // only ever touches files that never reached validate at all.
        assert!(result.files[0].is_valid, "{:?}", result.files[0].violations);
        assert!(!result.files[0].missing_frontmatter);
        assert_eq!(result.rollup.invalid, 0);
        assert_eq!(result.rollup.valid, 1);

        reclassify_exempt_missing(
            &mut result,
            &resolution.exempt,
            &resolution.profile,
            root.path(),
        );
        assert!(result.files[0].is_valid);
        assert_eq!(result.rollup.valid, 1);
        assert_eq!(result.rollup.missing_frontmatter, 0);
    }
}
