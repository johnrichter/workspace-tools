//! One file's frontmatter-conformance verdict against the merged profile --
//! factored out of `search` so `find` (M4.P2.T2) can reuse the identical
//! check for its own two-tier partition rather than re-deriving it.
//!
//! Delegates entirely to `frontmatter::validate` (the sole conformance
//! authority -- see `crate::lint`'s module doc) via the exact `rel_path`
//! contract `crate::lint::run` already established
//! ([`crate::lint::repo_relative_posix`]), so a file's verdict here is
//! identical to what `navigator lint` would report for it.

use std::path::Path;

use frontmatter::{validate, ParsedFrontmatter, Profile};
use serde::Serialize;

use crate::lint::repo_relative_posix;

/// One frontmatter schema violation -- mirrors [`frontmatter::Violation`]'s
/// three fields verbatim, reshaped for serialization (`Violation` itself
/// derives no `Serialize`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Violation {
    pub code: String,
    pub field: String,
    pub message: String,
}

/// A file's conformance verdict: whether it's schema-conformant, and every
/// violation found (empty when conformant) -- the two-tier partition key
/// plus its explanation.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub conformant: bool,
    pub violations: Vec<Violation>,
}

/// Validates `parsed` (found at `path`) against `profile`, deriving
/// `rel_path` from `repo_root` exactly as `navigator lint` does.
#[must_use]
pub fn check(
    parsed: &ParsedFrontmatter,
    path: &Path,
    repo_root: &Path,
    profile: &Profile,
) -> Verdict {
    let rel_path = repo_relative_posix(path, repo_root);
    let entry = validate(parsed, &rel_path, profile);
    Verdict {
        conformant: entry.is_valid,
        violations: entry
            .violations
            .into_iter()
            .map(|v| Violation {
                code: v.code,
                field: v.field,
                message: v.message,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn parse(text: &str) -> ParsedFrontmatter {
        frontmatter::parse(text).expect("test fixture must parse")
    }

    #[test]
    fn conformant_frontmatter_has_no_violations() {
        let repo_root = TempDir::new().unwrap();
        let parsed = parse(concat!(
            "---\n",
            "name: \"Doc\"\n",
            "description: \"A conformant test document.\"\n",
            "id: \"knowledge-base:test:doc\"\n",
            "tags:\n",
            "  - type:knowledge\n",
            "  - topic:testing\n",
            "  - status:complete\n",
            "  - privacy:internal\n",
            "  - owner:datadog\n",
            "links: []\n",
            "updated: 2026-07-11T00:00:00Z\n",
            "---\n",
            "body\n",
        ));
        let profile = crate::test_support::default_profile_for_tests();
        let verdict = check(
            &parsed,
            &repo_root.path().join("doc.md"),
            repo_root.path(),
            &profile,
        );
        assert!(verdict.conformant, "{:?}", verdict.violations);
        assert!(verdict.violations.is_empty());
    }

    #[test]
    fn nonconformant_frontmatter_reports_its_violations() {
        let repo_root = TempDir::new().unwrap();
        let parsed = parse("---\nname: \"x\"\n---\nbody\n");
        let profile = crate::test_support::default_profile_for_tests();
        let verdict = check(
            &parsed,
            &repo_root.path().join("doc.md"),
            repo_root.path(),
            &profile,
        );
        assert!(!verdict.conformant);
        assert!(!verdict.violations.is_empty());
    }
}
