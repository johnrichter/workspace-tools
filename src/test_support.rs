//! Test-only fixtures shared across this crate's unit tests -- a
//! self-authored synthetic extension pack, not a real shipped bundle, so a
//! test here exercises generic schema mechanics (cascade, report-only
//! namespaces, date-interval ranges) without coupling to any one named
//! pack's vocabulary. Mirrors the shape of `rust/frontmatter`'s own
//! `test_support::SYNTHETIC_PACK_JSON` so this crate's pre-existing test
//! literals (`feature:`/`product:`/`suite:`, `type:report`, `period:...`,
//! etc.) keep exercising the same cascade and rule-set paths.

use frontmatter::Profile;

/// Versioned distinctly from any embedded bundle so a test built against it
/// can never be confused with an artifact-acceptance test against a real
/// shipped pack.
const SYNTHETIC_PACK_JSON: &str = r#"{
    "kind": "extension-pack",
    "version": "navigator-test@1",
    "extends": "core@2.0.0",
    "required_fields": [
        { "field": "name", "authorship": "human_authored" },
        { "field": "description", "authorship": "human_authored" },
        { "field": "id", "authorship": "human_authored" },
        { "field": "tags", "authorship": "human_authored" },
        { "field": "links", "authorship": "human_authored" },
        { "field": "updated", "authorship": "machine_derivable" }
    ],
    "description_caps": { "context": 350, "skill": 500, "agent": 750 },
    "file_class": {
        "default": "context",
        "rules": [
            { "class": "skill", "match": { "glob": "**/SKILL.md" } },
            { "class": "agent", "match": { "glob": ".claude/agents/*.md" } }
        ]
    },
    "namespaces": [
        { "name": "type", "cardinality": "singleton" },
        { "name": "status", "cardinality": "singleton" },
        { "name": "privacy", "cardinality": "singleton" },
        { "name": "owner", "cardinality": "singleton" },
        { "name": "topic", "cardinality": "at_least_one" },
        { "name": "feature", "cardinality": "optional", "parents": ["product", "suite"] },
        { "name": "product", "cardinality": "optional", "parents": ["suite"] },
        { "name": "suite", "cardinality": "optional" },
        { "name": "source", "cardinality": "optional" },
        { "name": "period", "cardinality": "optional", "type": "date_interval" },
        { "name": "audience", "cardinality": "optional" },
        { "name": "cadence", "cardinality": "optional" }
    ],
    "rule_sets": [
        {
            "match": { "namespace": "type", "value": "report" },
            "apply": {
                "require_namespaces": ["source", "period"],
                "forbidden_unless_matched": ["source", "period", "audience", "cadence"],
                "value_formats": [
                    { "namespace": "period", "regex": "^[0-9]{4}-[0-9]{2}-[0-9]{2}/[0-9]{4}-[0-9]{2}-[0-9]{2}$", "message": "'{value}' is not YYYY-MM-DD/YYYY-MM-DD" }
                ]
            }
        }
    ],
    "exempt": {
        "filenames": ["plan.md", "execution.md", "CLAUDE.md"],
        "dir_components": [".pytest_cache", "__pycache__", ".git", "node_modules"],
        "path_globs": []
    }
}"#;

/// Builds a `Profile` from the embedded core plus [`SYNTHETIC_PACK_JSON`] --
/// this crate's drop-in replacement for the frontmatter library's own
/// (now-removed) bundled test profile.
///
/// # Panics
/// Never: `SYNTHETIC_PACK_JSON` is a committed literal in this file, checked
/// by this crate's own test suite.
pub(crate) fn default_profile_for_tests() -> Profile {
    Profile::from_pack_json(SYNTHETIC_PACK_JSON)
        .expect("SYNTHETIC_PACK_JSON must deserialize into a valid Profile")
}
