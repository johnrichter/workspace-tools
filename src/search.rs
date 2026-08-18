//! `navigator search`: `facetquery@1`-matched, BM25F-ranked search over the
//! reachable `.md` set, partitioned into a strict two-tier ranking by
//! frontmatter conformance.
//!
//! # Matching vs. ranking
//! The positional query (plus desugared `--tag`/`--type` sugar, see
//! [`crate::querybuild::build_query`]) decides WHICH files match, evaluated
//! per file via `frontmatter::matches` against the merged [`Profile`] --
//! matching is a `facetquery` concern end to end, not a text-similarity
//! score. [`bm25`] BM25F only ranks the matched set, scored against the
//! query's bareword/phrase terms alone (see
//! [`crate::querybuild::bareword_terms`]) -- a pure facet predicate
//! (`--tag`, or `topic:apm` written directly) narrows the match set but
//! contributes no text to rank by. `--path` stays a pre-filter (never a
//! `facetquery` predicate): it restricts which files are even considered,
//! exactly as before this rework.
//!
//! # Two-tier partition
//! Every conformant hit (per [`crate::conformance::check`] against the same
//! merged profile) ranks strictly above every nonconformant hit, regardless
//! of score; within a tier, BM25F score ranks descending, path ascending
//! breaks any tie. A file matching the query but failing conformance is
//! never excluded -- unlike the pre-rework tag filter's namespace-parent
//! exclusion, it is demoted to the bottom tier and flagged (`conformant:
//! false`, `violations`), findable but visibly in need of `navigator fix`.
//!
//! # Parse errors vs. eval diagnostics
//! A malformed positional query is a hard stop: [`build_query`] returns
//! `Err` before any file is even considered, and [`run`]'s `Err` carries the
//! positioned [`facetquery::ParseError`] text verbatim (`main.rs` maps this
//! to `USAGE_ERROR`, exit 2). A syntactically valid query with a semantic
//! issue against THIS profile (an unknown facet, a range against a
//! non-ordered facet) is instead an [`facetquery::EvalDiagnostic`] -- the
//! query still runs; the returned `Vec<EvalDiagnostic>` (from
//! [`querybuild::diagnostics`], shared with `find`) carries every diagnostic
//! for the caller to render as a warning.
//!
//! # Degraded notice
//! [`SearchResult`]'s top-level `degraded`/`degraded_reason`/`fix_command`
//! fire when at least one hit is nonconformant, when the caller's profile
//! resolution skipped a broken extension pack ([`run`]'s
//! `pack_degraded_reason`), or both -- reasons combine. Never suppressible
//! (`main.rs` never gates this behind `--quiet-schema-warnings`): a caller
//! must always be told their results include content that needs fixing,
//! independent of whether they've opted out of the separate, cosmetic
//! merge-warning/material-effect notes.
//!
//! # Determinism
//! - The corpus order is [`crate::scan::scan`]'s own deterministic order;
//!   matching and conformance-checking preserve that order into `eligible`.
//! - Two-tier sort is total: `(tier, score.total_cmp().reverse(), path)`.
//!   No `HashMap`/`HashSet` ever drives output order -- only point lookups.
//! - [`SearchResult`] is the one payload both JSON and terse rendering
//!   derive from ([`render_terse`] reads the same value [`run`] returns),
//!   so the two modes can never report a different result.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bm25::{BM25FConfig, BM25FDocument, BM25FIndex, Tokenizer};
use facetquery::EvalDiagnostic;
use frontmatter::{ParsedFrontmatter, Profile};
use serde::Serialize;

use crate::cli::SearchArgs;
use crate::conformance::{self, Violation};
use crate::filter;
use crate::querybuild::{self, build_query};
use crate::scan::ScannedFile;

/// The frontmatter-derived fields this command indexes, in the exact order
/// [`bm25::BM25FConfig::frontmatter_default`] registers them. Needed here,
/// separately from that config, only to compute each hit's `matched` list
/// (which fields the query's bareword terms hit) -- a per-field fact
/// BM25F's aggregate score alone doesn't expose.
const FIELD_NAMES: [&str; 5] = ["name", "id", "tags", "description", "body"];

/// One ranked, conformance-flagged search hit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub score: f64,
    pub hint: Option<String>,
    pub matched: Vec<String>,
    pub tags: Vec<String>,
    pub id: Option<String>,
    /// Whether this file conforms to the merged profile, per
    /// [`crate::conformance::check`] -- the two-tier partition key.
    pub conformant: bool,
    /// Every conformance violation found; empty iff `conformant`.
    pub violations: Vec<Violation>,
}

/// `search`'s full result -- the ranked hits, plus the degraded-state
/// notice (see the module doc). JSON and terse rendering both read this one
/// value, so the two can never report a different result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResult {
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
    pub hits: Vec<SearchHit>,
}

/// Renders `result` as one line per hit (`path — hint  [tags...]`, a
/// nonconformant hit's line additionally carrying a `NONCONFORMANT:<code>`
/// marker naming its first violation) followed, when degraded, by one
/// summary line naming the reason and the fix command. Reads the exact
/// value [`run`] returns -- the mechanism that keeps terse output
/// identical in content and order to the JSON encoding of the same value.
// Retained as a test-only shape/determinism helper: the shipped output is
// the clikit `ResultRecord` on stdout, never a terse human summary.
#[cfg(test)]
fn render_terse(result: &SearchResult) -> String {
    let mut lines: Vec<String> = result.hits.iter().map(render_hit_line).collect();
    if result.degraded {
        let reason = result.degraded_reason.as_deref().unwrap_or("");
        let fix_command = result.fix_command.as_deref().unwrap_or("");
        lines.push(format!("degraded: {reason} (run `{fix_command}`)"));
    }
    lines.join("\n")
}

#[cfg(test)]
fn render_hit_line(hit: &SearchHit) -> String {
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

/// This field's text for `parsed`, given its already-joined `tags_text`
/// (tag tokens space-joined so the field tokenizes like any other text
/// field). Unknown field names -- unreachable given [`FIELD_NAMES`] is this
/// function's only caller's source of field names -- yield an empty field
/// rather than panicking.
fn field_text<'a>(field: &str, parsed: &'a ParsedFrontmatter, tags_text: &'a str) -> &'a str {
    match field {
        "name" => parsed.name.as_deref().unwrap_or(""),
        "id" => parsed.id.as_deref().unwrap_or(""),
        "tags" => tags_text,
        "description" => parsed.description.as_deref().unwrap_or(""),
        "body" => parsed.body_text.as_str(),
        _ => "",
    }
}

/// Runs `args`' search over `scanned` (the pre-computed corpus from
/// [`crate::scan::scan`]) against `profile` (the merged profile resolved
/// for `repo_root`), returning the ranked, conformance-partitioned result
/// plus every eval-time diagnostic the query raised against `profile` (for
/// the caller to render as a warning -- see the module doc).
///
/// `pack_degraded_reason` is the caller's own profile-resolution degraded
/// reason (from [`crate::profile_resolve::Resolution::degraded`] in
/// [`crate::profile_resolve::ResolveMode::Discovery`]), folded into
/// [`SearchResult::degraded`] alongside any nonconformant hit; `None` when
/// resolution wasn't degraded.
///
/// `Err` only for a malformed `--path` glob or a malformed positional
/// query (see [`build_query`]) -- in either case nothing runs. Every other
/// input (an empty query, zero matching files) yields `Ok` with an empty
/// `hits`, never an error.
// One cohesive ranking pipeline (candidate filter -> match -> BM25F index ->
// two-tier partition); splitting it would only scatter the borrows the
// intermediate `Vec`s must outlive.
#[allow(clippy::too_many_lines)]
pub fn run(
    scanned: &[ScannedFile],
    args: &SearchArgs,
    repo_root: &Path,
    profile: &Profile,
    pack_degraded_reason: Option<&str>,
) -> Result<(SearchResult, Vec<EvalDiagnostic>), String> {
    let tokenizer = if args.whole_identifier {
        Tokenizer::WholeIdentifier
    } else {
        Tokenizer::CaseSplit
    };

    let path_glob = filter::compile_path_glob(args.path.as_deref())?;
    let query = build_query(&args.query, &args.tag, args.file_type.as_deref())
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

    // Path strings and joined tag text must outlive the `BM25FDocument`
    // borrows built from them below.
    let path_strings: Vec<String> = eligible
        .iter()
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect();
    let tags_texts: Vec<String> = eligible
        .iter()
        .map(|(_, parsed)| parsed.tags.join(" "))
        .collect();

    let config = BM25FConfig::frontmatter_default();
    let docs: Vec<BM25FDocument> = eligible
        .iter()
        .enumerate()
        .map(|(i, (_, parsed))| BM25FDocument {
            id: &path_strings[i],
            fields: FIELD_NAMES
                .iter()
                .map(|&field| (field, field_text(field, parsed, &tags_texts[i])))
                .collect(),
        })
        .collect();

    // Free-text score input is the query's bareword/phrase terms only --
    // a pure facet predicate narrowed the match set but has no text to
    // rank by (see the module doc).
    let bareword_text = querybuild::bareword_terms(&query.expr)
        .iter()
        .map(|term| term.raw.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let query_tokens: HashSet<String> = tokenizer.tokenize(&bareword_text).into_iter().collect();

    let index = BM25FIndex::build(&config, tokenizer, docs);
    // BM25F only returns docs scoring > 0.0 (see bm25::BM25FIndex::search);
    // every other matched doc defaults to 0.0 below, so no matched file is
    // ever dropped for lack of a free-text hit.
    let ranked = index.search(&bareword_text, eligible.len());
    let mut scores = vec![0.0_f64; eligible.len()];
    let index_of_path: std::collections::HashMap<&str, usize> = path_strings
        .iter()
        .enumerate()
        .map(|(i, path)| (path.as_str(), i))
        .collect();
    for scored in &ranked {
        if let Some(&i) = index_of_path.get(scored.id.as_str()) {
            scores[i] = scored.score;
        }
    }

    let mut hits: Vec<SearchHit> = eligible
        .iter()
        .enumerate()
        .map(|(i, (_, parsed))| {
            let matched: Vec<String> = FIELD_NAMES
                .iter()
                .filter(|&&field| {
                    tokenizer
                        .tokenize(field_text(field, parsed, &tags_texts[i]))
                        .iter()
                        .any(|token| query_tokens.contains(token))
                })
                .copied()
                .map(String::from)
                .collect();
            let verdict = conformance::check(parsed, eligible[i].0, repo_root, profile);
            SearchHit {
                path: path_strings[i].clone(),
                score: scores[i],
                hint: parsed.description.clone(),
                matched,
                tags: parsed.tags.clone(),
                id: parsed.id.clone(),
                conformant: verdict.conformant,
                violations: verdict.violations,
            }
        })
        .collect();

    // Strict two-tier partition: every conformant hit above every
    // nonconformant hit, BM25F score descending then path ascending within
    // each tier -- a total order, no HashMap iteration involved.
    hits.sort_by(|a, b| {
        let tier = (!a.conformant).cmp(&!b.conformant);
        tier.then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.path.cmp(&b.path))
    });

    if let Some(limit) = args.limit {
        hits.truncate(limit);
    }

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
    let result = SearchResult {
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
    use std::fs;
    use tempfile::TempDir;

    fn write_md(root: &std::path::Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }

    fn args(query: &str) -> SearchArgs {
        SearchArgs {
            query: query.to_string(),
            tag: Vec::new(),
            file_type: None,
            limit: None,
            path: None,
            dir: Vec::new(),
            whole_identifier: false,
        }
    }

    /// Scans a fixture tree with a fresh cache and returns the corpus
    /// [`run`] expects -- the same substrate `main.rs` builds in production.
    fn scan_fixture(root: &std::path::Path) -> Vec<ScannedFile> {
        let cache_base = TempDir::new().unwrap();
        let mut cache = FreshnessCache::open_in(cache_base.path(), root);
        crate::scan::scan(&[root.to_path_buf()], &mut cache)
    }

    /// Runs `a` against `root`'s scanned corpus with the synthetic test
    /// profile, discarding diagnostics -- the shape most tests below need.
    fn run_hits(root: &std::path::Path, a: &SearchArgs) -> Result<Vec<SearchHit>, String> {
        let scanned = scan_fixture(root);
        let profile = crate::test_support::default_profile_for_tests();
        run(&scanned, a, root, &profile, None).map(|(result, _)| result.hits)
    }

    /// Minimal frontmatter that also satisfies every synthetic-profile required-field
    /// check, so a fixture built with this is conformant unless it
    /// deliberately breaks something else.
    fn conformant_frontmatter(name: &str, description: &str, tags: &[&str]) -> String {
        let mut tags_yaml = String::new();
        for t in tags {
            tags_yaml.push_str("  - ");
            tags_yaml.push_str(t);
            tags_yaml.push('\n');
        }
        format!(
            "---\nname: \"{name}\"\ndescription: \"{description}\"\nid: \"knowledge-base:test:{name}\"\ntags:\n{tags_yaml}links: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n"
        )
    }

    fn build_corpus(root: &std::path::Path) {
        write_md(
            root,
            "high.md",
            &conformant_frontmatter(
                "dd_trace helper",
                "unrelated",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:internal",
                    "owner:datadog",
                ],
            ),
        );
        write_md(
            root,
            "low.md",
            "---\nname: \"unrelated\"\ndescription: \"unrelated\"\ntags:\n  - type:knowledge\n---\nbody mentions dd_trace once\n",
        );
        write_md(
            root,
            "no_match.md",
            "---\nname: \"unrelated\"\ndescription: \"unrelated\"\ntags:\n  - type:skill\n---\nbody unrelated\n",
        );
        write_md(root, "malformed.md", "---\ntags: [unclosed\n---\nbody\n");
    }

    // -- facetquery matching: bareword, facet predicates, sugar ------------

    #[test]
    fn tag_sugar_restricts_the_match_set() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let mut a = args("unrelated");
        a.tag = vec!["type:knowledge".to_string()];
        let hits = run_hits(root.path(), &a).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.iter().all(|p| !p.ends_with("no_match.md")));
        assert!(paths.iter().any(|p| p.ends_with("low.md")));
    }

    #[test]
    fn type_flag_is_a_tag_alias() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let mut a = args("unrelated");
        a.file_type = Some("skill".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("no_match.md"));
    }

    #[test]
    fn repeated_tag_flags_and_together() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let mut a = args("unrelated");
        a.tag = vec!["type:knowledge".to_string(), "type:skill".to_string()];
        let hits = run_hits(root.path(), &a).unwrap();
        assert!(hits.is_empty(), "no file has both tags");
    }

    #[test]
    fn positional_facet_predicate_is_equivalent_to_tag_sugar() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let via_sugar = {
            let mut a = args("unrelated");
            a.tag = vec!["type:knowledge".to_string()];
            run_hits(root.path(), &a).unwrap()
        };
        let via_positional = run_hits(root.path(), &args("unrelated AND type:knowledge")).unwrap();
        let sugar_paths: Vec<&str> = via_sugar.iter().map(|h| h.path.as_str()).collect();
        let positional_paths: Vec<&str> = via_positional.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(sugar_paths, positional_paths);
    }

    #[test]
    fn type_flag_is_byte_identical_to_positional_type_predicate() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let via_flag = {
            let mut a = args("unrelated");
            a.file_type = Some("knowledge".to_string());
            let scanned = scan_fixture(root.path());
            let profile = crate::test_support::default_profile_for_tests();
            run(&scanned, &a, root.path(), &profile, None).unwrap().0
        };
        let via_positional = {
            let scanned = scan_fixture(root.path());
            let profile = crate::test_support::default_profile_for_tests();
            run(
                &scanned,
                &args("unrelated AND type:knowledge"),
                root.path(),
                &profile,
                None,
            )
            .unwrap()
            .0
        };
        assert_eq!(
            serde_json::to_string(&via_flag).unwrap(),
            serde_json::to_string(&via_positional).unwrap(),
            "--type sugar must be byte-identical to the equivalent positional predicate"
        );
    }

    #[test]
    fn tag_flag_is_byte_identical_to_positional_facet_predicate() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let via_flag = {
            let mut a = args("unrelated");
            a.tag = vec!["type:knowledge".to_string()];
            let scanned = scan_fixture(root.path());
            let profile = crate::test_support::default_profile_for_tests();
            run(&scanned, &a, root.path(), &profile, None).unwrap().0
        };
        let via_positional = {
            let scanned = scan_fixture(root.path());
            let profile = crate::test_support::default_profile_for_tests();
            run(
                &scanned,
                &args("unrelated AND type:knowledge"),
                root.path(),
                &profile,
                None,
            )
            .unwrap()
            .0
        };
        assert_eq!(
            serde_json::to_string(&via_flag).unwrap(),
            serde_json::to_string(&via_positional).unwrap(),
            "--tag sugar must be byte-identical to the equivalent positional predicate"
        );
    }

    #[test]
    fn name_and_tags_outrank_body_for_the_same_term() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let hits = run_hits(root.path(), &args("dd_trace")).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths.len(), 2, "malformed.md and no_match.md never match");
        assert!(
            paths[0].ends_with("high.md"),
            "a name-field match must outrank a body-only match: {paths:?}"
        );
        assert!(paths[1].ends_with("low.md"));
    }

    // -- Two-tier partition: conformance above score ------------------------

    #[test]
    fn nonconformant_hit_ranks_below_every_conformant_hit_regardless_of_score() {
        let root = TempDir::new().unwrap();
        // Nonconformant (missing required fields) but a strong text match.
        write_md(
            root.path(),
            "aaa_strong_but_broken.md",
            "---\nname: \"target target target\"\n---\nbody\n",
        );
        // Conformant but a weaker text match (one hit vs three).
        write_md(
            root.path(),
            "zzz_weak_but_conformant.md",
            &conformant_frontmatter(
                "target",
                "unrelated",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:internal",
                    "owner:datadog",
                ],
            ),
        );

        let hits = run_hits(root.path(), &args("target")).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].path.ends_with("zzz_weak_but_conformant.md"),
            "conformant tier must rank first even with a lower score: {hits:?}"
        );
        assert!(hits[0].conformant);
        assert!(!hits[1].conformant);
        assert!(!hits[1].violations.is_empty());
    }

    #[test]
    fn orphan_namespace_tag_matches_but_is_demoted_not_excluded() {
        // The pre-rework behavior excluded a namespace-incoherent feature
        // tag outright; this rework matches it (facetquery has no parent
        // constraint) and demotes it via ORPHAN_NAMESPACE_TAG instead.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "incoherent.md",
            "---\nname: \"target\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags:\n  - type:knowledge\n  - status:complete\n  - privacy:internal\n  - owner:datadog\n  - topic:t\n  - feature:trace-explorer\nlinks: []\nupdated: 2026-07-11T00:00:00Z\n---\nbody\n",
        );

        let mut a = args("target");
        a.tag = vec!["feature:trace-explorer".to_string()];
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].conformant);
        assert!(hits[0]
            .violations
            .iter()
            .any(|v| v.code == "ORPHAN_NAMESPACE_TAG"));
    }

    // -- Parse errors vs. eval diagnostics -----------------------------------

    #[test]
    fn malformed_positional_query_is_a_positioned_error_and_nothing_runs() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let err = run(&scanned, &args("(unclosed"), root.path(), &profile, None).unwrap_err();
        assert!(
            err.contains("line 1"),
            "expected a positioned diagnostic: {err}"
        );
    }

    #[test]
    fn unknown_facet_is_an_eval_diagnostic_and_the_query_still_runs() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let (result, diagnostics) = run(
            &scanned,
            &args("not_a_real_namespace:x"),
            root.path(),
            &profile,
            None,
        )
        .unwrap();
        assert!(result.hits.is_empty(), "no file has that facet");
        assert_eq!(
            diagnostics,
            vec![EvalDiagnostic::UnknownFacet(
                "not_a_real_namespace".to_string()
            )]
        );
    }

    #[test]
    fn range_on_a_string_facet_is_an_eval_diagnostic_not_a_parse_error() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let (_, diagnostics) = run(
            &scanned,
            &args("topic:[a TO z]"),
            root.path(),
            &profile,
            None,
        )
        .unwrap();
        assert!(diagnostics.iter().any(
            |d| matches!(d, EvalDiagnostic::RangeOnNonOrdered { facet, .. } if facet == "topic")
        ));
    }

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

        let hits = run_hits(root.path(), &args("period:[2026-01-01 TO 2026-12-31]")).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("in_range.md"));
    }

    #[test]
    fn period_range_overlap_semantics_participate_in_ranking() {
        // A stored `date_interval` facet (`period:2026-04-01/2026-06-30`)
        // matches a query window under every overlap case, and not at all
        // when disjoint -- ranked alongside a plain bareword hit to confirm
        // a period-only match still lands in the two-tier/score sort.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "interval.md",
            "---\nname: \"target\"\ntags:\n  - period:2026-04-01/2026-06-30\n---\nbody\n",
        );

        let contains = run_hits(root.path(), &args("period:[2026-01-01 TO 2026-12-31]")).unwrap();
        assert_eq!(contains.len(), 1, "window contains the whole interval");

        let single_day = run_hits(root.path(), &args("period:2026-05-15")).unwrap();
        assert_eq!(single_day.len(), 1, "a day inside the interval matches");

        let partial = run_hits(root.path(), &args("period:[2026-06-01 TO 2026-08-01]")).unwrap();
        assert_eq!(partial.len(), 1, "a partially overlapping window matches");

        let disjoint = run_hits(root.path(), &args("period:[2027-01-01 TO 2027-12-31]")).unwrap();
        assert!(disjoint.is_empty(), "a disjoint window must not match");
    }

    #[test]
    fn range_on_a_string_facet_still_runs_the_query_alongside_its_diagnostic() {
        // Complements `range_on_a_string_facet_is_an_eval_diagnostic_not_a_parse_error`
        // (which only asserts the diagnostic) -- a nonordered-facet range is a
        // warning, not a hard stop, so the query must still execute and return
        // whatever bareword/other predicates in the same query would match.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "matches_bareword.md",
            "---\nname: \"target\"\ntags:\n  - topic:apm\n---\nbody\n",
        );

        let (result, diagnostics) = {
            let scanned = scan_fixture(root.path());
            let profile = crate::test_support::default_profile_for_tests();
            run(
                &scanned,
                &args("target OR topic:[a TO z]"),
                root.path(),
                &profile,
                None,
            )
            .unwrap()
        };
        assert_eq!(result.hits.len(), 1, "the bareword arm alone still matches");
        assert!(result.hits[0].path.ends_with("matches_bareword.md"));
        assert!(
            !diagnostics.is_empty(),
            "the range-on-string arm still warns"
        );
    }

    #[test]
    fn wildcard_term_matches_as_a_prefix() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "matches.md",
            "---\nname: \"tracehelper\"\n---\nbody\n",
        );
        write_md(
            root.path(),
            "no_match.md",
            "---\nname: \"unrelated\"\n---\nbody\n",
        );

        let hits = run_hits(root.path(), &args("trace*")).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("matches.md"));
    }

    // -- Degraded notice ------------------------------------------------------

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
        let (result, _) = run(&scanned, &args("target"), root.path(), &profile, None).unwrap();
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
                "d",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:internal",
                    "owner:datadog",
                ],
            ),
        );

        let hits = run_hits(root.path(), &args("target")).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].conformant);

        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let (result, _) = run(&scanned, &args("target"), root.path(), &profile, None).unwrap();
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
                "d",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:internal",
                    "owner:datadog",
                ],
            ),
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let (result, _) = run(
            &scanned,
            &args("target"),
            root.path(),
            &profile,
            Some("skipped pack(s): broken.json"),
        )
        .unwrap();
        assert!(result.degraded);
        assert!(result.degraded_reason.unwrap().contains("broken.json"));
    }

    #[test]
    fn terse_degraded_line_names_the_reason_and_fix_command() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "broken.md",
            "---\nname: \"target\"\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let (result, _) = run(&scanned, &args("target"), root.path(), &profile, None).unwrap();

        let terse = render_terse(&result);
        let last_line = terse.lines().last().unwrap();
        assert!(last_line.starts_with("degraded:"));
        assert!(last_line.contains("navigator fix ."));
    }

    #[test]
    fn terse_nonconformant_hit_line_carries_a_marker() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "broken.md",
            "---\nname: \"target\"\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();
        let (result, _) = run(&scanned, &args("target"), root.path(), &profile, None).unwrap();

        let terse = render_terse(&result);
        let hit_line = terse.lines().next().unwrap();
        assert!(hit_line.contains("NONCONFORMANT:"));
    }

    // -- Determinism / JSON-terse parity --------------------------------------

    #[test]
    fn repeated_runs_are_byte_identical() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let (first, _) = run(&scanned, &args("dd_trace"), root.path(), &profile, None).unwrap();
        let (second, _) = run(&scanned, &args("dd_trace"), root.path(), &profile, None).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
    }

    #[test]
    fn equal_scores_and_equal_tiers_tie_break_on_path() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "zzz.md", "---\nname: \"target\"\n---\nbody\n");
        write_md(root.path(), "aaa.md", "---\nname: \"target\"\n---\nbody\n");

        let hits = run_hits(root.path(), &args("target")).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].path < hits[1].path,
            "equal scores in the same tier must tie-break by path ascending: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn sub_token_match_under_default_tokenizer_but_not_whole_identifier() {
        let root = TempDir::new().unwrap();
        // `description` carries an exact, case-sensitive "trace" so the
        // file is eligible (facetquery bareword matching is case-sensitive
        // substring, independent of the BM25F tokenizer under test) under
        // BOTH tokenizer choices; `name`'s camelCase "ddTrace" is what
        // differs -- only the case-splitting tokenizer sub-tokenizes it
        // into a `matched`-worthy "trace".
        write_md(
            root.path(),
            "camel.md",
            "---\nname: \"ddTrace helper\"\ndescription: \"also mentions trace directly\"\n---\nbody\n",
        );

        let default_hits = run_hits(root.path(), &args("trace")).unwrap();
        assert_eq!(default_hits.len(), 1);
        assert!(
            default_hits[0].matched.contains(&"name".to_string()),
            "case-split must sub-match trace inside ddTrace: {:?}",
            default_hits[0].matched
        );

        let mut whole = args("trace");
        whole.whole_identifier = true;
        let whole_hits = run_hits(root.path(), &whole).unwrap();
        assert_eq!(
            whole_hits.len(),
            1,
            "still eligible via description's exact trace"
        );
        assert!(
            !whole_hits[0].matched.contains(&"name".to_string()),
            "whole-identifier mode must not sub-match trace inside ddTrace: {:?}",
            whole_hits[0].matched
        );
    }

    #[test]
    fn json_and_terse_report_the_same_hit_set_and_order() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());
        let scanned = scan_fixture(root.path());
        let profile = crate::test_support::default_profile_for_tests();

        let (result, _) = run(&scanned, &args("dd_trace"), root.path(), &profile, None).unwrap();
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
    fn hint_is_the_description_verbatim() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "a.md",
            "---\nname: \"target\"\ndescription: \"exact hint text\"\n---\nbody\n",
        );

        let hits = run_hits(root.path(), &args("target")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hint, Some("exact hint text".to_string()));
    }

    #[test]
    fn limit_caps_results_after_ranking_and_partitioning() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "a.md", "---\nname: \"target\"\n---\nbody\n");
        write_md(root.path(), "b.md", "---\nname: \"target\"\n---\nbody\n");
        write_md(root.path(), "c.md", "---\nname: \"target\"\n---\nbody\n");

        let mut a = args("target");
        a.limit = Some(2);
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn limit_cutting_across_the_tier_boundary_still_keeps_conformant_first() {
        // Two conformant hits (weaker text match) plus three nonconformant
        // hits (much stronger text match) -- a limit smaller than the
        // nonconformant tier alone must never let a nonconformant hit
        // displace a conformant one, however much higher its raw score.
        let root = TempDir::new().unwrap();
        for name in ["a1", "a2", "a3"] {
            write_md(
                root.path(),
                &format!("{name}_broken.md"),
                "---\nname: \"target target target\"\n---\nbody\n",
            );
        }
        for name in ["b1", "b2"] {
            write_md(
                root.path(),
                &format!("{name}_ok.md"),
                &conformant_frontmatter(
                    "target",
                    "d",
                    &[
                        "type:knowledge",
                        "topic:testing",
                        "status:complete",
                        "privacy:internal",
                        "owner:datadog",
                    ],
                ),
            );
        }

        let mut a = args("target");
        a.limit = Some(3);
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 3);
        let conformant_count = hits.iter().filter(|h| h.conformant).count();
        assert_eq!(
            conformant_count, 2,
            "both conformant hits must survive the cut, ahead of any nonconformant one: {hits:?}"
        );
        assert!(hits[0].conformant && hits[1].conformant && !hits[2].conformant);
    }

    #[test]
    fn path_glob_restricts_to_matching_files() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "docs/a.md",
            "---\nname: \"target\"\n---\nbody\n",
        );
        write_md(
            root.path(),
            "other/b.md",
            "---\nname: \"target\"\n---\nbody\n",
        );

        let mut a = args("target");
        a.path = Some("**/docs/**".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.contains("docs"));
    }

    #[test]
    fn path_glob_composes_with_the_two_tier_partition() {
        // A path pre-filter excludes a matching-but-out-of-scope file
        // entirely (never demotes it); what remains in scope still
        // partitions conformant-first among itself.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "docs/broken.md",
            "---\nname: \"target target target\"\n---\nbody\n",
        );
        write_md(
            root.path(),
            "docs/ok.md",
            &conformant_frontmatter(
                "target",
                "d",
                &[
                    "type:knowledge",
                    "topic:testing",
                    "status:complete",
                    "privacy:internal",
                    "owner:datadog",
                ],
            ),
        );
        // Out of the --path scope, would otherwise outscore everything.
        write_md(
            root.path(),
            "other/loud.md",
            "---\nname: \"target target target target target\"\n---\nbody\n",
        );

        let mut a = args("target");
        a.path = Some("**/docs/**".to_string());
        let hits = run_hits(root.path(), &a).unwrap();
        assert_eq!(hits.len(), 2, "other/loud.md must be excluded by --path");
        assert!(
            hits.iter().all(|h| h.path.contains("docs")),
            "path filter must not leak an out-of-scope file in: {hits:?}"
        );
        assert!(hits[0].conformant && hits[0].path.ends_with("ok.md"));
        assert!(!hits[1].conformant && hits[1].path.ends_with("broken.md"));
    }

    #[test]
    fn malformed_frontmatter_never_appears_in_ranked_results() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let hits = run_hits(root.path(), &args("body")).unwrap();
        assert!(hits.iter().all(|h| !h.path.ends_with("malformed.md")));
    }

    #[test]
    fn invalid_path_glob_is_reported_as_an_error() {
        let root = TempDir::new().unwrap();

        let mut a = args("target");
        a.path = Some("[unclosed".to_string());
        assert!(run_hits(root.path(), &a).is_err());
    }

    #[test]
    fn query_matching_nothing_is_an_empty_ok_not_an_error() {
        let root = TempDir::new().unwrap();
        build_corpus(root.path());

        let hits = run_hits(root.path(), &args("zzz_nonexistent_term_zzz")).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn unicode_and_emoji_frontmatter_does_not_panic() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            "unicode.md",
            "---\nname: \"caf\u{e9} \u{1f680} target\"\ndescription: \"Jos\u{e9} \u{1f600}\"\n---\nbody\n",
        );

        let hits = run_hits(root.path(), &args("target")).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("unicode.md"));
    }

    #[test]
    fn every_field_name_is_actually_scored_by_the_bm25_config() {
        // `frontmatter::matches`'s bareword eligibility (`text_matches`)
        // deliberately never reads `tags` -- only `name`/`id`/`description`/
        // `body` (see that crate's `FrontmatterFacetSource::text_fields`).
        // So a `tags`-only bareword term is never even eligible; unlike the
        // other four fields, its fixture also places the term in `body`
        // (for eligibility) and checks BM25F's own field-tokenizing (the
        // `matched` computation, independent of eligibility) still credits
        // `tags`.
        for field in FIELD_NAMES {
            let root = TempDir::new().unwrap();
            let unique_token = format!("uniquetoken{field}marker");
            let contents = match field {
                "name" => format!("---\nname: \"{unique_token}\"\n---\nbody\n"),
                "id" => format!("---\nid: \"{unique_token}\"\n---\nbody\n"),
                "tags" => format!("---\ntags:\n  - \"{unique_token}\"\n---\n{unique_token}\n"),
                "description" => format!("---\ndescription: \"{unique_token}\"\n---\nbody\n"),
                "body" => format!("---\nname: \"unrelated\"\n---\n{unique_token}\n"),
                other => panic!("FIELD_NAMES entry {other:?} has no test-corpus case"),
            };
            write_md(root.path(), "doc.md", &contents);

            let hits = run_hits(root.path(), &args(&unique_token)).unwrap();
            assert_eq!(
                hits.len(),
                1,
                "field {field:?}: a term placed only in this field must be found -- \
                 an empty result means FIELD_NAMES names a field the bm25 config never scores"
            );
            assert!(
                hits[0].matched.contains(&field.to_string()),
                "field {field:?}: matched list {:?} should name the field the term hit",
                hits[0].matched
            );
        }
    }
}
