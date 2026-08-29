//! `navigator find`: `facetquery@1`-matched lookup over the reachable `.md`
//! set, conformance-partitioned like `search` -- but never ranked.
//!
//! # Matching, no ranking
//! The positional query (plus desugared `--tag`/`--type` sugar, see
//! [`crate::querybuild::build_query`]) decides WHICH files match, evaluated
//! per file via `frontmatter::matches` against the merged [`Profile`] --
//! identical matching semantics to `search`. Unlike `search`, `find` never
//! scores or ranks the matched set: it is a deterministic filter, not a
//! relevance search, so a hit carries no `score`. `--path` stays a
//! pre-filter (never a `facetquery` predicate), exactly as `search` treats
//! it.
//!
//! # Two-tier partition
//! Every conformant hit (per [`crate::conformance::check`] against the same
//! merged profile) ranks strictly above every nonconformant hit; within a
//! tier, path sorts ascending -- `find` has no score to break ties with, so
//! path alone decides order inside each tier. A file matching the query but
//! failing conformance -- including a namespace parent-constraint violation
//! (`feature:` without its required `product:`/`suite:`) -- is never
//! excluded: conformance demotes it to the bottom tier and flags it
//! (`conformant: false`, `violations`) rather than being dropped by the
//! pre-rework exclusion-based parent filter this replaces.
//!
//! # Parse errors vs. eval diagnostics
//! Identical contract to `search`: a malformed positional query is a hard
//! stop ([`build_query`]'s `Err`, mapped by `main.rs` to `USAGE_ERROR`, exit
//! 2, before any file is considered). A syntactically valid query with a
//! semantic issue against THIS profile (an unknown facet, a range against a
//! non-ordered facet) is instead an [`facetquery::EvalDiagnostic`] -- the
//! query still runs; [`querybuild::diagnostics`] (shared with `search`)
//! computes every diagnostic once per run, not once per file.
//!
//! # Degraded notice
//! [`FindResult`]'s top-level `degraded`/`degraded_reason`/`fix_command`
//! fire on the same two conditions `search` uses: at least one hit is
//! nonconformant, or the caller's profile resolution skipped a broken
//! extension pack ([`run`]'s `pack_degraded_reason`). Never suppressible.
//!
//! # Determinism
//! - The corpus order is [`crate::scan::scan`]'s own deterministic order;
//!   matching and conformance-checking preserve that order into `eligible`.
//! - Two-tier sort is total: `(tier, path)` -- no `HashMap`/`HashSet` ever
//!   drives output order.
//! - [`FindResult`] is the one payload both JSON and terse rendering derive
//!   from, so the two modes can never report a different result.

use std::path::{Path, PathBuf};

use frontmatter::{ParsedFrontmatter, Profile};
use serde::Serialize;

use crate::cli::FindArgs;
use crate::conformance::{self, Violation};
use crate::filter;
use crate::querybuild::{self, build_query};
use crate::scan::ScannedFile;

/// One conformance-flagged, unranked find hit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FindHit {
    pub path: String,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    pub tags: Vec<String>,
    pub hint: Option<String>,
    /// Whether this file conforms to the merged profile, per
    /// [`crate::conformance::check`] -- the two-tier partition key.
    pub conformant: bool,
    /// Every conformance violation found; empty iff `conformant`.
    pub violations: Vec<Violation>,
}

/// `find`'s full result -- the conformance-partitioned hits, plus the
/// degraded-state notice (see the module doc). JSON and terse rendering both
/// read this one value, so the two modes can never report a different
/// result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FindResult {
    /// `true` iff at least one hit is nonconformant, or the profile
    /// resolution behind this run skipped a broken extension pack. Never
    /// suppressible.
    pub degraded: bool,
    /// Why `degraded` is `true`; every contributing reason joined, `None`
    /// when not degraded.
    pub degraded_reason: Option<String>,
    /// The command a caller should run to address `degraded_reason`;
    /// `None` when not degraded.
    pub fix_command: Option<String>,
    pub hits: Vec<FindHit>,
}

/// Renders `result` as one line per hit (`path — hint  [tags...]`, a
/// nonconformant hit additionally carrying a `NONCONFORMANT:<code>` marker
/// naming its first violation) followed, when degraded, by one summary
/// line -- the same shape `search::render_terse` uses, over the exact value
/// [`run`] returns.
// Retained as a test-only shape/determinism helper: the shipped output is
// the clikit `ResultRecord` on stdout, never a terse human summary.
#[cfg(test)]
fn render_terse(result: &FindResult) -> String {
    let mut lines: Vec<String> = result.hits.iter().map(render_hit_line).collect();
    if result.degraded {
        let reason = result.degraded_reason.as_deref().unwrap_or("");
        let fix_command = result.fix_command.as_deref().unwrap_or("");
        lines.push(format!("degraded: {reason} (run `{fix_command}`)"));
    }
    lines.join("\n")
}

#[cfg(test)]
fn render_hit_line(hit: &FindHit) -> String {
    let hint_text = hit.hint.as_deref().unwrap_or("");
    let tags = if hit.tags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", hit.tags.join(" "))
    };
    let marker = if hit.conformant {
        String::new()
    } else {
        let code = hit
            .violations
            .first()
            .map_or("UNKNOWN", |v| v.code.as_str());
        format!("  NONCONFORMANT:{code}")
    };
    format!("{} — {hint_text}{tags}{marker}", hit.path)
}

/// The value half of a `namespace:value` tag whose namespace is `namespace`,
/// e.g. `tag_value(tags, "type")` reads a file's `type:` tag's value.
/// `None` if `tags` carries no tag in that namespace (the first in source
/// order wins if more than one). Surfaces [`FindHit::file_type`] only --
/// the actual `--type`/`--tag` FILTERING goes through [`build_query`]'s
/// `facetquery` sugar, not this helper.
fn tag_value(tags: &[String], namespace: &str) -> Option<String> {
    let prefix = format!("{namespace}:");
    tags.iter()
        .find_map(|tag| tag.strip_prefix(&prefix).map(str::to_string))
}

/// Runs `args`' find over `scanned` (the pre-computed corpus from
/// [`crate::scan::scan`]) against `profile` (the merged profile resolved
/// for `repo_root`), returning the conformance-partitioned result plus
/// every eval-time diagnostic the query raised against `profile`.
///
/// `pack_degraded_reason` mirrors `search::run`'s parameter of the same
/// name -- the caller's own profile-resolution degraded reason, folded into
/// [`FindResult::degraded`] alongside any nonconformant hit.
///
/// `Err` only for a malformed `--path` glob or a malformed positional query
/// (see [`build_query`]) -- in either case nothing runs. Every other input
/// (an empty query, zero matching files) yields `Ok` with an empty `hits`.
pub fn run(
    scanned: &[ScannedFile],
    args: &FindArgs,
    repo_root: &Path,
    profile: &Profile,
    pack_degraded_reason: Option<&str>,
) -> Result<(FindResult, Vec<facetquery::EvalDiagnostic>), String> {
    let path_glob = filter::compile_path_glob(args.path.as_deref())?;
    let query = build_query(
        args.query.as_deref().unwrap_or(""),
        &args.tag,
        args.file_type.as_deref(),
    )
    .map_err(|err| format!("query: {err}"))?;

    let diagnostics = querybuild::diagnostics(&query, profile);

    // --path stays a pre-filter (never a facetquery predicate); everything
    // else is decided by facetquery matching against the merged profile.
    let candidates: Vec<(&PathBuf, &ParsedFrontmatter)> = scanned
        .iter()
        .filter_map(|file| file.parsed.as_ref().ok().map(|parsed| (&file.path, parsed)))
        .filter(|(path, _)| filter::passes_path_glob(path, path_glob.as_ref()))
        .collect();

    let eligible: Vec<(&PathBuf, &ParsedFrontmatter)> = candidates
        .into_iter()
        .filter(|(_, parsed)| frontmatter::matches(parsed, &query, profile).matched)
        .collect();

    let mut hits: Vec<FindHit> = eligible
        .into_iter()
        .map(|(path, parsed)| {
            let verdict = conformance::check(parsed, path, repo_root, profile);
            FindHit {
                path: path.to_string_lossy().into_owned(),
                id: parsed.id.clone(),
                file_type: tag_value(&parsed.tags, "type"),
                tags: parsed.tags.clone(),
                hint: parsed.description.clone(),
                conformant: verdict.conformant,
                violations: verdict.violations,
            }
        })
        .collect();

    // Strict two-tier partition: every conformant hit above every
    // nonconformant hit, path ascending breaks every tie within a tier --
    // a total order, no HashMap iteration involved, no score to compare.
    hits.sort_by(|a, b| {
        let tier = (!a.conformant).cmp(&!b.conformant);
        tier.then_with(|| a.path.cmp(&b.path))
    });

    let nonconformant = hits.iter().filter(|h| !h.conformant).count();
    let mut reasons = Vec::new();
    if let Some(reason) = pack_degraded_reason {
        reasons.push(reason.to_string());
    }
    if nonconformant > 0 {
        reasons.push(format!(
            "{nonconformant} of {} hit(s) are nonconformant",
            hits.len()
        ));
    }
    let degraded = !reasons.is_empty();
    let result = FindResult {
        degraded,
        degraded_reason: degraded.then(|| reasons.join("; ")),
        fix_command: degraded.then(|| "navigator fix .".to_string()),
        hits,
    };

    Ok((result, diagnostics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FreshnessCache;
    use facetquery::EvalDiagnostic;
    use std::fs;
    use tempfile::TempDir;

    fn write_md(root: &std::path::Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }

    fn args() -> FindArgs {
        FindArgs {
            query: None,
            file_type: None,
            tag: Vec::new(),
            path: None,
            dir: Vec::new(),
        }
    }

    fn scan_fixture(root: &std::path::Path) -> Vec<ScannedFile> {
        let cache_base = TempDir::new().unwrap();
        let mut cache = FreshnessCache::open_in(cache_base.path(), root);
        crate::scan::scan(&[root.to_path_buf()], &mut cache)
    }

    /// Runs `a` against `root`'s scanned corpus with the synthetic test
    /// profile, discarding diagnostics -- the shape most tests below need.
    fn run_hits(root: &std::path::Path, a: &FindArgs) -> Result<Vec<FindHit>, String> {
        let scanned = scan_fixture(root);
        let profile = crate::test_support::default_profile_for_tests();
        run(&scanned, a, root, &profile, None).map(|(result, _)| result.hits)
    }

    /// Minimal frontmatter that also satisfies every synthetic-profile required-field
    /// check, so a fixture built with this is conformant unless it
    /// deliberately breaks something else.
    fn conformant_frontmatter(name: &str, tags: &[&str]) -> String {
        let mut tags_yaml = String::new();
        for t in tags {
            tags_yaml.push_str("  - ");
            tags_yaml.push_str(t);
            tags_yaml.push('\n');
        }
        format!(
            "---\nname: \"{name}\"\ndescription: \"d\"\nid: \"knowledge-base:test:{name}\"\ntags:\n{tags_yaml}links: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n"
        )
    }

    // -- facetquery matching: tag/type sugar, exact membership --------------

    #[test]
    fn exact_tag_and_excludes_a_file_matching_only_some_tags() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "both.md",
            "---\ntags:\n  - type:skill\n  - topic:apm\n---\nbody\n",
        );
        write_md(
            root.path(),
            "one.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );

        let mut a = args();
        a.tag = vec!["type:skill".to_string(), "topic:apm".to_string()];
        let hits = run_hits(root.path(), &a).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("both.md"));
    }

    #[test]
    fn type_flag_is_a_tag_alias() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "a.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );
        write_md(
            root.path(),
            "b.md",
            "---\ntags:\n  - type:knowledge\n---\nbody\n",
        );

        let mut a = args();
        a.file_type = Some("skill".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("a.md"));
        assert_eq!(hits[0].file_type, Some("skill".to_string()));
    }

    #[test]
    fn positional_query_composes_with_tag_sugar() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "match.md",
            "---\nname: \"target\"\ntags:\n  - type:skill\n---\nbody\n",
        );
        write_md(
            root.path(),
            "wrong_type.md",
            "---\nname: \"target\"\ntags:\n  - type:knowledge\n---\nbody\n",
        );

        let mut a = args();
        a.query = Some("target".to_string());
        a.file_type = Some("skill".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("match.md"));
    }

    // -- Cardinality/parent constraints: demoted, never excluded ------------

    #[test]
    fn orphan_namespace_tag_matches_but_is_demoted_not_excluded() {
        // The pre-rework behavior excluded a namespace-incoherent feature
        // tag outright; this rework matches it (facetquery has no parent
        // constraint) and demotes it via ORPHAN_NAMESPACE_TAG instead --
        // conformance subsumes the old exclusion-based parent filter.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "incoherent.md",
            "---\nname: \"target\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags:\n  - type:knowledge\n  - status:complete\n  - privacy:example\n  - owner:example\n  - topic:t\n  - feature:trace-explorer\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n",
        );

        let mut a = args();
        a.query = Some("target".to_string());
        a.tag = vec!["feature:trace-explorer".to_string()];
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "the orphan tag must still match, not be excluded"
        );
        assert!(!hits[0].conformant);
        assert!(hits[0]
            .violations
            .iter()
            .any(|v| v.code == "ORPHAN_NAMESPACE_TAG"));
    }

    #[test]
    fn coherent_parent_chain_is_conformant() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "coherent.md",
            &conformant_frontmatter(
                "target",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:example",
                    "owner:example",
                    "feature:trace-explorer",
                    "product:apm-tracing",
                    "suite:apm",
                ],
            ),
        );

        let mut a = args();
        a.tag = vec!["feature:trace-explorer".to_string()];
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].conformant);
    }

    // -- Range predicates -----------------------------------------------------

    #[test]
    fn date_range_matches_a_date_typed_facet() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "in_range.md",
            "---\nname: \"target\"\ntags:\n  - period:2026-04-01/2026-06-30\n---\nbody\n",
        );
        write_md(
            root.path(),
            "out_of_range.md",
            "---\nname: \"target\"\ntags:\n  - period:2020-01-01/2020-01-31\n---\nbody\n",
        );

        let mut a = args();
        a.query = Some("period:[2026-01-01 TO 2026-12-31]".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("in_range.md"));
    }

    #[test]
    fn period_range_overlap_semantics_hold_across_every_case() {
        // A stored `date_interval` facet (`period:2026-04-01/2026-06-30`)
        // matches under containment, a single day inside it, and a partial
        // overlap, and does not match a disjoint window -- the same
        // conformant-first two-tier + path sort still applies to a
        // period-only hit.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "interval.md",
            "---\nname: \"target\"\ntags:\n  - period:2026-04-01/2026-06-30\n---\nbody\n",
        );

        let mut contains_args = args();
        contains_args.query = Some("period:[2026-01-01 TO 2026-12-31]".to_string());
        let contains = run_hits(root.path(), &contains_args).unwrap();
        assert_eq!(contains.len(), 1, "window contains the whole interval");

        let mut single_day_args = args();
        single_day_args.query = Some("period:2026-05-15".to_string());
        let single_day = run_hits(root.path(), &single_day_args).unwrap();
        assert_eq!(single_day.len(), 1, "a day inside the interval matches");

        let mut partial_args = args();
        partial_args.query = Some("period:[2026-06-01 TO 2026-08-01]".to_string());
        let partial = run_hits(root.path(), &partial_args).unwrap();
        assert_eq!(partial.len(), 1, "a partially overlapping window matches");

        let mut disjoint_args = args();
        disjoint_args.query = Some("period:[2027-01-01 TO 2027-12-31]".to_string());
        let disjoint = run_hits(root.path(), &disjoint_args).unwrap();
        assert!(disjoint.is_empty(), "a disjoint window must not match");
    }

    #[test]
    fn nonconformant_file_matching_a_period_query_is_included_and_demoted_not_dropped() {
        // The two-tier partition (conformant-first, see module doc) applies
        // to a period-only match exactly like any other predicate: a file
        // whose `date_interval` overlaps the query window but which is
        // otherwise missing required frontmatter still matches and lands in
        // the results, demoted below any conformant hit -- never dropped.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "broken_but_in_range.md",
            "---\nname: \"broken\"\ntags:\n  - period:2026-04-01/2026-06-30\n---\nbody\n",
        );
        write_md(
            root.path(),
            "zzz_ok_in_range.md",
            &conformant_frontmatter(
                "ok",
                &[
                    "type:report",
                    "topic:testing",
                    "status:complete",
                    "privacy:example",
                    "owner:example",
                    "source:slack",
                    "period:2026-05-01/2026-05-31",
                ],
            ),
        );

        let mut a = args();
        a.query = Some("period:[2026-01-01 TO 2026-12-31]".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "the nonconformant period-only match must not be dropped: {hits:?}"
        );
        assert!(
            hits[0].path.ends_with("zzz_ok_in_range.md"),
            "conformant hit ranks first regardless of path: {hits:?}"
        );
        assert!(hits[0].conformant);
        assert!(
            hits[1].path.ends_with("broken_but_in_range.md"),
            "nonconformant period match is demoted to the bottom tier: {hits:?}"
        );
        assert!(!hits[1].conformant);
    }

    #[test]
    fn range_on_a_string_facet_is_an_eval_diagnostic_and_the_query_still_runs() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "matches_bareword.md",
            "---\nname: \"target\"\ntags:\n  - topic:apm\n---\nbody\n",
        );

        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args();
        a.query = Some("target OR topic:[a TO z]".to_string());
        let (result, diagnostics) = run(&scanned, &a, root.path(), &profile, None).unwrap();
        assert_eq!(result.hits.len(), 1, "the bareword arm alone still matches");
        assert!(diagnostics.iter().any(
            |d| matches!(d, EvalDiagnostic::RangeOnNonOrdered { facet, .. } if facet == "topic")
        ));
    }

    // -- Parse errors ---------------------------------------------------------

    #[test]
    fn malformed_positional_query_is_a_positioned_error_and_nothing_runs() {
        let root = TempDir::new().unwrap();
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let mut a = args();
        a.query = Some("(unclosed".to_string());
        let err = run(&scanned, &a, root.path(), &profile, None).unwrap_err();
        assert!(
            err.contains("line 1"),
            "expected a positioned diagnostic: {err}"
        );
    }

    // -- Two-tier partition: conformant-first, then path -----------------------

    #[test]
    fn nonconformant_hit_ranks_below_every_conformant_hit() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "aaa_broken.md",
            "---\nname: \"target\"\n---\nbody\n",
        );
        write_md(
            root.path(),
            "zzz_ok.md",
            &conformant_frontmatter(
                "target",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:example",
                    "owner:example",
                ],
            ),
        );

        let mut a = args();
        a.query = Some("target".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].path.ends_with("zzz_ok.md"),
            "conformant tier must rank first regardless of path: {hits:?}"
        );
        assert!(hits[0].conformant);
        assert!(!hits[1].conformant);
    }

    #[test]
    fn equal_tier_ties_break_on_path() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "zzz.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );
        write_md(
            root.path(),
            "aaa.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );

        let mut a = args();
        a.file_type = Some("skill".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].path < hits[1].path);
    }

    // -- Degraded notice --------------------------------------------------------

    #[test]
    fn degraded_notice_fires_on_a_nonconformant_hit_and_names_a_fix_command() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "broken.md",
            "---\nname: \"target\"\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let mut a = args();
        a.query = Some("target".to_string());
        let (result, _) = run(&scanned, &a, root.path(), &profile, None).unwrap();
        assert!(result.degraded);
        assert!(result.degraded_reason.unwrap().contains("nonconformant"));
        assert_eq!(result.fix_command, Some("navigator fix .".to_string()));
    }

    #[test]
    fn degraded_notice_absent_when_every_hit_is_conformant() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "ok.md",
            &conformant_frontmatter(
                "target",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:example",
                    "owner:example",
                ],
            ),
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let mut a = args();
        a.query = Some("target".to_string());
        let (result, _) = run(&scanned, &a, root.path(), &profile, None).unwrap();
        assert!(!result.degraded);
        assert_eq!(result.degraded_reason, None);
        assert_eq!(result.fix_command, None);
    }

    #[test]
    fn degraded_notice_folds_in_the_callers_pack_degraded_reason() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "ok.md",
            &conformant_frontmatter(
                "target",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:example",
                    "owner:example",
                ],
            ),
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let mut a = args();
        a.query = Some("target".to_string());
        let (result, _) = run(
            &scanned,
            &a,
            root.path(),
            &profile,
            Some("skipped pack(s): broken.json"),
        )
        .unwrap();
        assert!(result.degraded);
        assert!(result.degraded_reason.unwrap().contains("broken.json"));
    }

    #[test]
    fn terse_nonconformant_hit_line_carries_a_marker_and_degraded_summary() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "broken.md",
            "---\nname: \"target\"\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let mut a = args();
        a.query = Some("target".to_string());
        let (result, _) = run(&scanned, &a, root.path(), &profile, None).unwrap();

        let terse = render_terse(&result);
        let lines: Vec<&str> = terse.lines().collect();
        assert!(lines[0].contains("NONCONFORMANT:"));
        assert!(lines.last().unwrap().starts_with("degraded:"));
        assert!(lines.last().unwrap().contains("navigator fix ."));
    }

    // -- JSON/terse parity, determinism -----------------------------------------

    #[test]
    fn json_and_terse_report_the_same_set_and_order() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "b.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );
        write_md(
            root.path(),
            "a.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let mut a = args();
        a.file_type = Some("skill".to_string());
        let (result, _) = run(&scanned, &a, root.path(), &profile, None).unwrap();
        let terse = render_terse(&result);
        let terse_paths: Vec<&str> = terse
            .lines()
            .filter(|l| !l.starts_with("degraded:"))
            .map(|line| line.split(" — ").next().unwrap())
            .collect();
        let json_paths: Vec<&str> = result.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(terse_paths, json_paths);
    }

    #[test]
    fn repeated_runs_are_byte_identical() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "a.md", "---\nname: \"target\"\n---\nbody\n");
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let mut a = args();
        a.query = Some("target".to_string());
        let (first, _) = run(&scanned, &a, root.path(), &profile, None).unwrap();
        let (second, _) = run(&scanned, &a, root.path(), &profile, None).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }

    // -- Unparseable frontmatter, path glob, hint ------------------------------

    #[test]
    fn unparseable_frontmatter_is_excluded() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "bad.md", "---\ntags: [unclosed\n---\nbody\n");

        let hits = run_hits(root.path(), &args()).unwrap();
        assert!(hits.iter().all(|h| !h.path.ends_with("bad.md")));
    }

    #[test]
    fn path_glob_restricts_to_matching_files() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "docs/a.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );
        write_md(
            root.path(),
            "other/b.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );

        let mut a = args();
        a.file_type = Some("skill".to_string());
        a.path = Some("**/docs/**".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.contains("docs"));
    }

    #[test]
    fn invalid_path_glob_is_reported_as_an_error() {
        let root = TempDir::new().unwrap();

        let mut a = args();
        a.path = Some("[unclosed".to_string());
        assert!(run_hits(root.path(), &a).is_err());
    }

    #[test]
    fn hint_is_the_description_verbatim() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "a.md",
            "---\ndescription: \"exact hint text\"\ntags:\n  - type:skill\n---\nbody\n",
        );

        let mut a = args();
        a.file_type = Some("skill".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hint, Some("exact hint text".to_string()));
    }

    #[test]
    fn query_matching_nothing_is_an_empty_ok_not_an_error() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "a.md",
            "---\ntags:\n  - type:skill\n---\nbody\n",
        );

        let mut a = args();
        a.query = Some("zzz_nonexistent_term_zzz".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert!(hits.is_empty());
    }

    /// The differentiator a plain-text `grep` can't express: a text search
    /// for `feature:trace-explorer` matches BOTH files below, but this
    /// query's namespace-aware conformance check demotes the one missing
    /// its required parent tags instead of excluding it -- unlike the
    /// pre-rework filter, the set itself is not narrowed by the parent
    /// constraint, only its ordering/flagging is.
    #[test]
    fn equivalence_to_a_naive_grep_the_query_subsumes_but_demotes_not_excludes() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "incoherent.md",
            "---\nname: \"target\"\ntags:\n  - feature:trace-explorer\n---\nbody\n",
        );
        write_md(
            root.path(),
            "coherent.md",
            "---\nname: \"target\"\ntags:\n  - feature:trace-explorer\n  - product:apm-tracing\n  - suite:apm\n---\nbody\n",
        );

        let mut a = args();
        a.tag = vec!["feature:trace-explorer".to_string()];
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "both match; the parent constraint demotes, not excludes"
        );
        let coherent = hits
            .iter()
            .find(|h| h.path.ends_with("coherent.md"))
            .unwrap();
        let incoherent = hits
            .iter()
            .find(|h| h.path.ends_with("incoherent.md"))
            .unwrap();
        assert!(!incoherent.conformant);
        assert!(incoherent
            .violations
            .iter()
            .any(|v| v.code == "ORPHAN_NAMESPACE_TAG"));
        assert!(!coherent
            .violations
            .iter()
            .any(|v| v.code == "ORPHAN_NAMESPACE_TAG"));
    }
}
