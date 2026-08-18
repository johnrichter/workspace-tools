//! The `navigator.toml` sentinel: the repo-root marker a repo commits to
//! opt in to navigator. Its absence is not an error -- it means "not
//! adopted, resolve to the neutral core-only floor" ([`profile_resolve`]).
//!
//! # Schema
//!
//! The sentinel has its own formal, versioned schema -- it is not a
//! free-form config. `sentinel_version` names the schema version this file
//! was written against, so the format can evolve without breaking repos
//! that adopted an earlier version. This module supports exactly the
//! versions listed in [`SUPPORTED_SENTINEL_VERSIONS`]; loading a file
//! declaring any other version is a clear error rather than a best-effort
//! parse.
//!
//! Version 2 fields:
//!
//! | Field | Required | Type | Meaning |
//! | --- | --- | --- | --- |
//! | `sentinel_version` | yes | integer | This file's own schema version. |
//! | `extensions` | yes | string or array of strings | Repo extension pack(s): a named bundle or a committed-file path each. A bare string is a single-element list; order is preserved. |
//! | `navigator_version` | no | string | Gate/CI binary version pin. Parsed and type-checked here only. |
//! | `schema.profile` | yes | string | Frontmatter profile + version this repo validates against, e.g. `"core@2.0.0"`. |
//! | `schema.suppress_merge_warnings` | no | boolean | Suppresses schema-pack merge override/removal warnings; defaults to `false`. |
//! | `lint.include` / `lint.exclude` | no | array of strings | Lint-scope glob hints. |
//! | `symbols.languages` | no | array of strings | Languages the symbols pass should cover. |
//!
//! Unknown fields anywhere in the file are rejected -- a typo in a field
//! name fails loudly instead of being silently ignored. The sentinel is a
//! small, versioned contract, not an open config surface.
//!
//! # Parsing
//!
//! Parsed through figment2's TOML provider (SC-STACK), the same config
//! mechanism the runtime knobs in [`crate::config`] resolve through -- this
//! crate never reaches for a raw TOML parser of its own. The
//! `sentinel_version` is read first through a permissive probe, then the
//! full strict schema is extracted only once the version is confirmed
//! supported: a file written against a future version -- which may
//! legitimately carry fields this build doesn't know -- fails as a clear
//! [`SentinelError::UnsupportedVersion`] ("upgrade navigator") rather than a
//! misleading unknown-field parse error.

use std::fmt;
use std::path::{Path, PathBuf};

use figment2::providers::{Format, Toml};
use figment2::Figment;
use serde::{Deserialize, Deserializer};

/// `sentinel_version` values this build of navigator understands.
pub const SUPPORTED_SENTINEL_VERSIONS: &[u64] = &[2];

/// The parsed, validated contents of a repo's `navigator.toml`. Field names
/// mirror the file's own TOML keys, including `sentinel_version` repeating
/// the struct name -- the sentinel format dictates that key, not this
/// crate's naming style.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
pub struct Sentinel {
    /// This file's own schema version. Validated against
    /// [`SUPPORTED_SENTINEL_VERSIONS`] during [`load`].
    pub sentinel_version: u64,

    /// The repo's extension pack(s), in declaration order. Accepts a bare
    /// string (normalized to a single-element list) or an array of strings.
    /// Opaque to this module; resolution is [`crate::profile_resolve`]'s job.
    #[serde(deserialize_with = "deserialize_extensions")]
    pub extensions: Vec<String>,

    /// The gate/CI binary version pin, when the repo pins one. Parsed and
    /// type-checked here only.
    #[serde(default)]
    pub navigator_version: Option<String>,

    /// The `[schema]` table: which frontmatter profile this repo validates
    /// against, and how schema-pack merge warnings behave.
    pub schema: SchemaConfig,

    /// Optional lint scope hints. Absent entirely when the repo declares no
    /// `[lint]` table.
    #[serde(default)]
    pub lint: Option<LintScope>,

    /// Optional symbols-pass scope hints. Absent entirely when the repo
    /// declares no `[symbols]` table.
    #[serde(default)]
    pub symbols: Option<SymbolsScope>,
}

/// `[schema]` table: the frontmatter profile this repo validates against,
/// and schema-pack merge warning behavior.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaConfig {
    /// Frontmatter profile + version this repo validates against, e.g.
    /// `"core@2.0.0"`. Opaque to this module.
    pub profile: String,

    /// Suppresses the schema-pack merge override/removal warnings. Governs
    /// merge warnings only -- never hard errors, never the degraded-state
    /// notice. Defaults to `false`.
    #[serde(default)]
    pub suppress_merge_warnings: bool,
}

/// Accepts a bare TOML string or an array of strings for `extensions`,
/// normalizing both to an ordered `Vec<String>`.
fn deserialize_extensions<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrList {
        One(String),
        Many(Vec<String>),
    }

    Ok(match StringOrList::deserialize(deserializer)? {
        StringOrList::One(s) => vec![s],
        StringOrList::Many(v) => v,
    })
}

/// `[lint]` scope hints: which files a lint pass should consider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintScope {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// `[symbols]` scope hints: which languages a symbols pass should cover.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolsScope {
    #[serde(default)]
    pub languages: Vec<String>,
}

/// A repo's declared relationship to navigator: either it committed a
/// `navigator.toml` (adopted, with the parsed sentinel), or it didn't (not
/// adopted, resolving to the neutral core-only floor). Modeled as an enum
/// rather than a bare `Option` so callers read the semantic at the call
/// site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adoption {
    /// The repo committed a `navigator.toml` that parsed and validated.
    Adopted(Sentinel),
    /// No `navigator.toml` at the repo root. Not an error.
    NotAdopted,
}

/// Everything that can go wrong loading a sentinel -- always specific enough
/// to name the file and the exact defect.
#[derive(Debug)]
pub enum SentinelError {
    /// The file exists but could not be read (permissions, non-UTF-8, a
    /// directory in its place -- anything other than "does not exist,"
    /// which is [`Adoption::NotAdopted`]).
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not well-formed TOML, or a field's type/shape doesn't
    /// match the schema (including an unknown field).
    Parse {
        path: PathBuf,
        source: Box<figment2::Error>,
    },
    /// The file parsed, but declares a `sentinel_version` this build does
    /// not support.
    UnsupportedVersion {
        path: PathBuf,
        found: u64,
        supported: &'static [u64],
    },
}

impl fmt::Display for SentinelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SentinelError::Io { path, source } => {
                write!(f, "failed to read sentinel {}: {source}", path.display())
            }
            SentinelError::Parse { path, source } => {
                write!(f, "invalid sentinel {}: {source}", path.display())
            }
            SentinelError::UnsupportedVersion {
                path,
                found,
                supported,
            } => write!(
                f,
                "sentinel {} declares sentinel_version = {found}, but this navigator build only \
                 supports {supported:?}. Upgrade navigator, or downgrade the sentinel_version in \
                 the repo's navigator.toml.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SentinelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SentinelError::Io { source, .. } => Some(source),
            SentinelError::Parse { source, .. } => Some(source.as_ref()),
            SentinelError::UnsupportedVersion { .. } => None,
        }
    }
}

/// Loads and strictly validates the `navigator.toml` at `repo_root`, if one
/// exists.
///
/// Returns `Ok(Adoption::NotAdopted)` when the file is simply absent -- the
/// expected, valid state for a repo that hasn't opted in. Any other failure
/// to read the file (permissions, non-UTF-8, a directory in its place) is a
/// real [`SentinelError::Io`], never silently treated as "not adopted."
///
/// # Errors
/// [`SentinelError`] for an unreadable present file, a malformed/invalid
/// sentinel, or an unsupported `sentinel_version`.
pub fn load(repo_root: &Path) -> Result<Adoption, SentinelError> {
    let path = repo_root.join("navigator.toml");

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Adoption::NotAdopted);
        }
        Err(source) => return Err(SentinelError::Io { path, source }),
    };

    let parse_err = |source: figment2::Error| SentinelError::Parse {
        path: path.clone(),
        source: Box::new(source),
    };

    let probe: VersionProbe = Figment::new()
        .merge(Toml::string(&contents))
        .extract()
        .map_err(parse_err)?;
    if !SUPPORTED_SENTINEL_VERSIONS.contains(&probe.sentinel_version) {
        return Err(SentinelError::UnsupportedVersion {
            path,
            found: probe.sentinel_version,
            supported: SUPPORTED_SENTINEL_VERSIONS,
        });
    }

    let sentinel: Sentinel = Figment::new()
        .merge(Toml::string(&contents))
        .extract()
        .map_err(parse_err)?;
    Ok(Adoption::Adopted(sentinel))
}

/// Minimal, permissive view used to read `sentinel_version` before the
/// strict full parse. Deliberately NOT `deny_unknown_fields`: it must
/// tolerate the extra fields a future schema version may add so it can reach
/// the version those fields belong to and report it as
/// [`SentinelError::UnsupportedVersion`].
#[derive(Deserialize)]
struct VersionProbe {
    sentinel_version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_sentinel(root: &Path, contents: &str) {
        fs::write(root.join("navigator.toml"), contents).unwrap();
    }

    #[test]
    fn full_sentinel_parses_to_expected_model() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            r#"
                sentinel_version = 2
                extensions = ["psa-apm@1", "psa-security@1"]
                navigator_version = "1.2.3"

                [schema]
                profile = "core@1"
                suppress_merge_warnings = true

                [lint]
                include = ["**/*.md"]
                exclude = ["reference-materials/code-repositories/**"]

                [symbols]
                languages = ["go", "python", "typescript", "rust"]
            "#,
        );

        let got = load(root.path()).unwrap();
        assert_eq!(
            got,
            Adoption::Adopted(Sentinel {
                sentinel_version: 2,
                extensions: vec!["psa-apm@1".to_string(), "psa-security@1".to_string()],
                navigator_version: Some("1.2.3".to_string()),
                schema: SchemaConfig {
                    profile: "core@1".to_string(),
                    suppress_merge_warnings: true,
                },
                lint: Some(LintScope {
                    include: vec!["**/*.md".to_string()],
                    exclude: vec!["reference-materials/code-repositories/**".to_string()],
                }),
                symbols: Some(SymbolsScope {
                    languages: vec![
                        "go".to_string(),
                        "python".to_string(),
                        "typescript".to_string(),
                        "rust".to_string(),
                    ],
                }),
            })
        );
    }

    #[test]
    fn minimal_sentinel_with_only_required_fields_works() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );

        let got = load(root.path()).unwrap();
        assert_eq!(
            got,
            Adoption::Adopted(Sentinel {
                sentinel_version: 2,
                extensions: vec!["psa-apm@1".to_string()],
                navigator_version: None,
                schema: SchemaConfig {
                    profile: "core@1".to_string(),
                    suppress_merge_warnings: false,
                },
                lint: None,
                symbols: None,
            })
        );
    }

    #[test]
    fn bare_string_and_array_extensions_yield_identical_vec() {
        let bare = TempDir::new().unwrap();
        write_sentinel(
            bare.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        let array = TempDir::new().unwrap();
        write_sentinel(
            array.path(),
            "sentinel_version = 2\nextensions = [\"psa-apm@1\"]\n\n[schema]\nprofile = \"core@1\"\n",
        );

        let Adoption::Adopted(bare) = load(bare.path()).unwrap() else {
            panic!("expected Adopted");
        };
        let Adoption::Adopted(array) = load(array.path()).unwrap() else {
            panic!("expected Adopted");
        };
        assert_eq!(bare.extensions, vec!["psa-apm@1".to_string()]);
        assert_eq!(bare.extensions, array.extensions);
    }

    #[test]
    fn extensions_array_preserves_declaration_order() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = [\"c-ext@1\", \"a-ext@1\", \"b-ext@1\"]\n\n[schema]\nprofile = \"core@1\"\n",
        );
        let Adoption::Adopted(sentinel) = load(root.path()).unwrap() else {
            panic!("expected Adopted");
        };
        assert_eq!(
            sentinel.extensions,
            vec![
                "c-ext@1".to_string(),
                "a-ext@1".to_string(),
                "b-ext@1".to_string()
            ]
        );
    }

    #[test]
    fn empty_extensions_array_parses_to_empty_vec() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = []\n\n[schema]\nprofile = \"core@1\"\n",
        );
        let Adoption::Adopted(sentinel) = load(root.path()).unwrap() else {
            panic!("expected Adopted");
        };
        assert_eq!(sentinel.extensions, Vec::<String>::new());
    }

    #[test]
    fn suppress_merge_warnings_defaults_to_false_and_round_trips_true() {
        let default = TempDir::new().unwrap();
        write_sentinel(
            default.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        let set = TempDir::new().unwrap();
        write_sentinel(
            set.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\nsuppress_merge_warnings = true\n",
        );

        let Adoption::Adopted(default) = load(default.path()).unwrap() else {
            panic!("expected Adopted");
        };
        let Adoption::Adopted(set) = load(set.path()).unwrap() else {
            panic!("expected Adopted");
        };
        assert!(!default.schema.suppress_merge_warnings);
        assert!(set.schema.suppress_merge_warnings);
    }

    #[test]
    fn typo_inside_schema_table_is_a_clear_parse_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\nsuppres_merge_warnings = true\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn sentinel_version_1_now_fails_as_unsupported_version() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 1\nschema = \"core@1\"\nextensions = \"psa-apm@1\"\n",
        );
        match load(root.path()).unwrap_err() {
            SentinelError::UnsupportedVersion { found, .. } => assert_eq!(found, 1),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_not_adopted_not_an_error() {
        let root = TempDir::new().unwrap();
        assert_eq!(load(root.path()).unwrap(), Adoption::NotAdopted);
    }

    #[test]
    fn unknown_field_is_a_clear_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\ntotally_made_up_field = \"oops\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        let err = load(root.path()).unwrap_err();
        assert!(matches!(err, SentinelError::Parse { .. }));
        assert!(err.to_string().contains("navigator.toml"));
    }

    #[test]
    fn misspelled_nested_field_is_a_clear_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n\n[lint]\ninclud = [\"**/*.md\"]\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn wrong_type_for_sentinel_version_is_a_clear_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = \"one\"\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn unsupported_sentinel_version_is_a_clear_versioning_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 999\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        let err = load(root.path()).unwrap_err();
        match &err {
            SentinelError::UnsupportedVersion { found, .. } => assert_eq!(*found, 999),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
        assert!(err.to_string().contains("999"));
    }

    // A FUTURE sentinel_version that also carries a field this build doesn't
    // know must fail as UnsupportedVersion (telling the operator to upgrade
    // navigator), NOT as an unknown-field parse error.
    #[test]
    fn future_version_with_unknown_field_is_unsupported_version_not_parse_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 3\nextensions = \"psa-apm@1\"\nfield_added_in_v3 = true\n\n[schema]\nprofile = \"core@1\"\n",
        );
        match load(root.path()).unwrap_err() {
            SentinelError::UnsupportedVersion { found, .. } => assert_eq!(found, 3),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn malformed_toml_is_a_clear_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(root.path(), "this is not [ valid toml");
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn missing_required_field_is_a_clear_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\n\n[schema]\nprofile = \"core@1\"\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn optional_sections_absent_leave_none() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        let Adoption::Adopted(sentinel) = load(root.path()).unwrap() else {
            panic!("expected Adopted");
        };
        assert_eq!(sentinel.navigator_version, None);
        assert_eq!(sentinel.lint, None);
        assert_eq!(sentinel.symbols, None);
    }

    // A present-but-unreadable sentinel (a directory sitting where the file
    // should be) must surface as Io, never be swallowed into NotAdopted.
    #[test]
    fn sentinel_path_that_is_a_directory_is_io_error_not_not_adopted() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("navigator.toml")).unwrap();
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Io { .. }
        ));
    }

    // Non-UTF-8 bytes must be a clean Io error, not a panic or NotAdopted.
    #[test]
    fn non_utf8_sentinel_bytes_are_a_clean_io_error_not_a_panic() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("navigator.toml"), [0xff, 0xfe, 0x00, 0xff]).unwrap();
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Io { .. }
        ));
    }

    #[test]
    fn duplicate_toml_key_is_a_clear_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 1\nsentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn empty_symbols_table_present_parses_to_some_with_empty_languages() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n\n[symbols]\n",
        );
        let Adoption::Adopted(sentinel) = load(root.path()).unwrap() else {
            panic!("expected Adopted");
        };
        assert_eq!(sentinel.symbols, Some(SymbolsScope { languages: vec![] }));
    }

    // The schema documents `schema.profile` and `extensions` entries as
    // opaque strings with no format constraint, so an empty string is
    // accepted rather than rejected. Pinned so it's a deliberate choice.
    #[test]
    fn empty_string_profile_and_extensions_entry_are_currently_accepted() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"\"\n\n[schema]\nprofile = \"\"\n",
        );
        let Adoption::Adopted(sentinel) = load(root.path()).unwrap() else {
            panic!("expected Adopted");
        };
        assert_eq!(sentinel.schema.profile, "");
        assert_eq!(sentinel.extensions, vec![String::new()]);
    }

    #[test]
    fn float_sentinel_version_is_a_clear_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 1.0\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn negative_sentinel_version_is_a_clear_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = -1\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn duplicate_extensions_entries_are_preserved_as_is() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = [\"psa-apm@1\", \"psa-apm@1\"]\n\n[schema]\nprofile = \"core@1\"\n",
        );
        let Adoption::Adopted(sentinel) = load(root.path()).unwrap() else {
            panic!("expected Adopted");
        };
        assert_eq!(
            sentinel.extensions,
            vec!["psa-apm@1".to_string(), "psa-apm@1".to_string()]
        );
    }

    #[test]
    fn extensions_as_wrong_scalar_type_is_a_clear_parse_error_not_a_panic() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = 123\n\n[schema]\nprofile = \"core@1\"\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn extensions_array_containing_non_string_element_is_a_clear_parse_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = [\"ok\", 5]\n\n[schema]\nprofile = \"core@1\"\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn schema_table_present_but_missing_profile_is_a_clear_parse_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nsuppress_merge_warnings = true\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }

    #[test]
    fn suppress_merge_warnings_with_non_bool_value_is_a_clear_parse_error() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            "sentinel_version = 2\nextensions = \"psa-apm@1\"\n\n[schema]\nprofile = \"core@1\"\nsuppress_merge_warnings = \"yes\"\n",
        );
        assert!(matches!(
            load(root.path()).unwrap_err(),
            SentinelError::Parse { .. }
        ));
    }
}
