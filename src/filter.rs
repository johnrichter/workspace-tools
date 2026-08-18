//! The `--path` glob pre-filter shared by `search` and `find` -- one
//! implementation so the two commands can never disagree about what
//! "matches `--path`" means.
//!
//! `--tag`/`--type` filtering (including namespace parent constraints, e.g.
//! `feature:` requiring `product:`/`suite:`) is no longer a pre-filter here:
//! both commands desugar it into a `facetquery@1` predicate (see
//! `crate::querybuild::build_query`) and let `crate::conformance::check`
//! flag a parent-constraint violation as a demotable nonconformance, never
//! an exclusion. `--path` stays the one pre-filter that's genuinely outside
//! `facetquery`'s scope -- it restricts which files are even considered,
//! not a predicate over any file's frontmatter.

use std::path::Path;

use globset::{Glob, GlobMatcher};

/// Compiles `--path`'s glob, if given. `Err` only for a pattern `globset`
/// rejects -- the one case both `search` and `find` report as a usage
/// error rather than an empty result.
pub fn compile_path_glob(pattern: Option<&str>) -> Result<Option<GlobMatcher>, String> {
    match pattern {
        Some(pattern) => Glob::new(pattern)
            .map(|glob| Some(glob.compile_matcher()))
            .map_err(|err| format!("invalid --path glob {pattern:?}: {err}")),
        None => Ok(None),
    }
}

/// True iff `path` matches `glob`, or `glob` is absent (no `--path`
/// restriction given).
#[must_use]
pub fn passes_path_glob(path: &Path, glob: Option<&GlobMatcher>) -> bool {
    glob.is_none_or(|glob| glob.is_match(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn no_glob_always_passes() {
        assert!(passes_path_glob(&PathBuf::from("any/path.md"), None));
    }

    #[test]
    fn glob_restricts_to_matching_paths() {
        let glob = compile_path_glob(Some("**/docs/**")).unwrap();
        assert!(passes_path_glob(
            &PathBuf::from("a/docs/b.md"),
            glob.as_ref()
        ));
        assert!(!passes_path_glob(
            &PathBuf::from("a/other/b.md"),
            glob.as_ref()
        ));
    }

    #[test]
    fn invalid_glob_is_reported_as_an_error() {
        assert!(compile_path_glob(Some("[unclosed")).is_err());
    }
}
