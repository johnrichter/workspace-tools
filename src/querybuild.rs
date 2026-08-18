//! Builds one combined `facetquery@1` [`Query`] from a positional query
//! string plus `--tag`/`--type` sugar, and extracts the bareword/phrase
//! terms a caller needs for free-text ranking.
//!
//! # Sugar, not a second filter path
//! `--tag KEY:VALUE` and `--type T` are convenience syntax for one more
//! AND-combined `facet:value` predicate, not a separate pre-filter pass -- so a
//! query like `topic:apm OR name:*trace*` combined with `--type knowledge`
//! reads exactly as `(topic:apm OR name:*trace*) AND type:knowledge`, with
//! the same eval-time semantics (unknown-facet diagnostics, type gating)
//! `facetquery` already applies to anything written directly in the
//! positional query. [`build_query`] is `search`'s only place this
//! combination happens, factored out so `find` (M4.P2.T2) builds the exact
//! same combined AST rather than re-deriving the sugar rule.
//!
//! # Positioned parse errors, unaffected
//! Only the positional query is parsed by [`facetquery::parse`] (which can
//! fail with a positioned [`facetquery::ParseError`]); `--tag`/`--type`
//! values are wrapped directly as AST predicates, never re-parsed as
//! query-language text, so a tag value containing a facetquery-reserved
//! character (whitespace, `"`, `:`, `*`, ...) can never itself trigger a
//! parse error.
//!
//! # Eval-diagnostic probe, also shared
//! [`diagnostics`] is the other seam `search` and `find` share: an
//! unknown-facet or range-on-non-ordered diagnostic depends only on the
//! query AST and the profile's schema, never on any one document's field
//! values, so probing once per run (against an empty synthetic document) is
//! exact, not an approximation -- both commands call this instead of
//! re-deriving the probe or re-running `facetquery::eval`'s diagnostic path
//! once per file for an identical answer each time.

use facetquery::{EvalDiagnostic, Expr, Matcher, Predicate, Query, Seg, Term};
use frontmatter::{ParsedFrontmatter, Profile, RawFields};

/// A literal (non-wildcard) [`Term`] over `value`, verbatim.
fn literal_term(value: &str) -> Term {
    Term {
        raw: value.to_string(),
        segments: vec![Seg::Literal(value.to_string())],
    }
}

/// One `--tag` value as an AND-combined predicate. `KEY:VALUE` becomes
/// `facet:Some(KEY)` matching `VALUE` literally; a colon-less value (never a
/// real tag in this corpus) becomes a bareword predicate over that literal
/// text instead, so it still composes into the query rather than being
/// rejected -- consistent with `--tag`/`--type` never producing an `Err`.
fn tag_predicate(tag: &str) -> Expr {
    let (facet, value) = match tag.split_once(':') {
        Some((facet, value)) => (Some(facet.to_string()), value),
        None => (None, tag),
    };
    Expr::Pred(Predicate {
        facet,
        matcher: Matcher::Term(literal_term(value)),
    })
}

/// Parses `positional` and ANDs it with every `--tag`/`--type` predicate
/// (folding `--type T` into `type:T`, the same alias [`crate::filter`] used
/// for the pre-facetquery filter). Returns `base` itself, unwrapped, when
/// there's no sugar to combine -- so a plain `navigator search foo` builds
/// exactly the AST `facetquery::parse("foo")` would. An empty (or
/// whitespace-only) `positional` parses to the canonical match-all
/// (`Expr::And(vec![])`, see [`facetquery::parse`]'s doc); when sugar is
/// present, that no-op conjunct is dropped rather than carried into the
/// combined AST, so `--tag topic:apm` alone builds exactly
/// `facetquery::parse("topic:apm")` would, not a needless
/// `And([And([]), topic:apm])`.
///
/// # Errors
/// Only `positional` can fail to parse; `--tag`/`--type` desugaring never
/// fails (see [`tag_predicate`]).
pub fn build_query(
    positional: &str,
    tag: &[String],
    file_type: Option<&str>,
) -> Result<Query, facetquery::ParseError> {
    let base = facetquery::parse(positional)?;

    let mut sugar: Vec<Expr> = tag.iter().map(|t| tag_predicate(t)).collect();
    if let Some(file_type) = file_type {
        sugar.push(tag_predicate(&format!("type:{file_type}")));
    }

    if sugar.is_empty() {
        return Ok(base);
    }

    let is_match_all = matches!(&base.expr, Expr::And(conjuncts) if conjuncts.is_empty());
    let mut conjuncts = if is_match_all {
        Vec::new()
    } else {
        vec![base.expr]
    };
    conjuncts.extend(sugar);

    Ok(if conjuncts.len() == 1 {
        Query {
            expr: conjuncts.remove(0),
        }
    } else {
        Query {
            expr: Expr::And(conjuncts),
        }
    })
}

/// Every bareword/phrase [`Term`] in `expr` -- a leaf `Predicate` with
/// `facet: None` -- in left-to-right traversal order. Excludes every
/// facet-scoped predicate (including `--tag`/`--type` sugar, which is
/// always facet-scoped): a free-text ranking/highlighting consumer wants
/// only the terms a user is searching FOR, not the filters narrowing what's
/// searched.
#[must_use]
pub fn bareword_terms(expr: &Expr) -> Vec<&Term> {
    let mut out = Vec::new();
    collect_bareword_terms(expr, &mut out);
    out
}

fn collect_bareword_terms<'a>(expr: &'a Expr, out: &mut Vec<&'a Term>) {
    match expr {
        Expr::And(exprs) | Expr::Or(exprs) => {
            for e in exprs {
                collect_bareword_terms(e, out);
            }
        }
        Expr::Not(inner) | Expr::Group(inner) => collect_bareword_terms(inner, out),
        Expr::Pred(Predicate {
            facet: None,
            matcher: Matcher::Term(term),
        }) => out.push(term),
        Expr::Pred(_) => {}
    }
}

/// An eval-diagnostic probe against `query` and `profile` alone, independent
/// of any real document -- an unknown-facet or range-on-non-ordered
/// diagnostic is raised purely from what the profile's schema declares
/// (never from a document's own field values), so this is exact, not an
/// approximation. `search` and `find` both call this once per run, before
/// iterating any file, rather than re-deriving the same probe or paying for
/// it once per file.
#[must_use]
pub fn diagnostics(query: &Query, profile: &Profile) -> Vec<EvalDiagnostic> {
    let probe = ParsedFrontmatter {
        tags: Vec::new(),
        name: None,
        id: None,
        description: None,
        body_text: String::new(),
        raw_fields: RawFields::from_ordered_pairs(Vec::new()),
    };
    frontmatter::matches(&probe, query, profile).diagnostics
}

/// One human-readable line for an [`EvalDiagnostic`] -- `EvalDiagnostic`
/// carries no `Display` of its own (the crate that defines it has no
/// rendering need), so this is navigator's own presentation, shared by
/// `search` and `find`'s stderr warning rendering.
#[must_use]
pub fn render_diagnostic(diagnostic: &EvalDiagnostic) -> String {
    match diagnostic {
        EvalDiagnostic::UnknownFacet(facet) => {
            format!("query facet {facet:?} is not a namespace this profile declares")
        }
        EvalDiagnostic::RangeOnNonOrdered { facet, ty } => format!(
            "query used a range/comparison against {facet:?}, whose type ({ty:?}) isn't ordered"
        ),
        // `EvalDiagnostic` is `#[non_exhaustive]`: a future facetquery
        // release may add a diagnostic kind this navigator version
        // predates. Render it generically rather than fail to build.
        other => format!("query diagnostic: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sugar_returns_the_parsed_positional_query_unwrapped() {
        let query = build_query("topic:apm", &[], None).unwrap();
        assert_eq!(query, facetquery::parse("topic:apm").unwrap());
    }

    #[test]
    fn tag_sugar_ands_a_facet_predicate_onto_the_positional_query() {
        let query = build_query("trace", &["topic:apm".to_string()], None).unwrap();
        let expected = facetquery::parse("trace AND topic:apm").unwrap();
        assert_eq!(query, expected);
    }

    #[test]
    fn type_sugar_folds_into_a_type_facet_predicate() {
        let query = build_query("trace", &[], Some("knowledge")).unwrap();
        let expected = facetquery::parse("trace AND type:knowledge").unwrap();
        assert_eq!(query, expected);
    }

    #[test]
    fn tag_and_type_sugar_combine_with_the_positional_query() {
        let query = build_query("trace", &["topic:apm".to_string()], Some("knowledge")).unwrap();
        let expected = facetquery::parse("trace AND topic:apm AND type:knowledge").unwrap();
        assert_eq!(query, expected);
    }

    #[test]
    fn sugar_with_an_empty_positional_query_still_builds_a_valid_query() {
        let query = build_query("", &["type:knowledge".to_string()], None).unwrap();
        let expected = facetquery::parse("type:knowledge").unwrap();
        assert_eq!(query, expected);
    }

    #[test]
    fn positional_parse_error_is_positioned_and_never_reaches_sugar() {
        let err = build_query("(unclosed", &["topic:apm".to_string()], None).unwrap_err();
        assert!(err.to_string().contains("line 1"));
    }

    #[test]
    fn bareword_terms_excludes_facet_scoped_predicates() {
        let query = build_query("trace helper", &["topic:apm".to_string()], None).unwrap();
        let terms: Vec<&str> = querybuild_test_terms(&query);
        assert_eq!(terms, vec!["trace", "helper"]);
    }

    #[test]
    fn bareword_terms_walks_boolean_combinators() {
        let query = facetquery::parse("(a OR b) AND NOT c AND topic:apm").unwrap();
        let terms: Vec<&str> = querybuild_test_terms(&query);
        assert_eq!(terms, vec!["a", "b", "c"]);
    }

    fn querybuild_test_terms(query: &Query) -> Vec<&str> {
        bareword_terms(&query.expr)
            .into_iter()
            .map(|t| t.raw.as_str())
            .collect()
    }
}
