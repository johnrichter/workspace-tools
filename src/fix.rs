//! `navigator fix`: the binary half of Tier-1 frontmatter repair.
//!
//! `frontmatter::propose_fix`/`propose_skeleton`/`render` are PURE -- no
//! filesystem, clock, or model call (see that crate's `fix.rs` module doc,
//! SC16). Everything this module adds is the I/O and side-effect layer
//! those functions deliberately push out to their caller:
//!
//! - computing `now` (the `updated:` stamp) from the wall clock,
//! - re-nesting managed fields under `workspace:` for a file class the
//!   external Claude Code schema owns (see [`is_externally_schemad_class`]),
//! - the actual read/write to disk, gated by `--apply` (default dry-run),
//! - an optional `--set FIELD=VALUE` mechanism for feeding an authored
//!   value back into a field the pure library could only stub (Tier-2's
//!   boundary -- this module never invents that value itself).
//!
//! # Dispatch
//! Per in-scope file: no frontmatter at all (parse failed, or `raw_fields`
//! is empty) -> [`frontmatter::propose_skeleton`]. Any frontmatter present
//! -> [`frontmatter::validate`] (for the report's `violations_before`) then
//! [`frontmatter::propose_fix`]. Both branches converge on one write
//! formula: `render(&fields) + parsed.body_text` -- `body_text` is already
//! "everything after the original block" (or the whole file, when there
//! was no block), so this single formula both inserts a skeleton and
//! replaces an existing block without any separate byte-splitting code.
//!
//! # Malformed frontmatter
//! A file whose `parse` itself failed (unclosed delimiter, invalid YAML)
//! has no usable `body_text` to preserve, so it is reported `Unfixable`
//! and never written -- attempting a fix here would risk silently
//! discarding content this crate never successfully parsed in the first
//! place.
//!
//! # Body preservation and the leading BOM
//! The write preserves the body byte-for-byte with one deliberate
//! normalization: `frontmatter::parse` strips a single leading UTF-8 BOM
//! before producing `body_text` (so a BOM-saved file's frontmatter isn't
//! silently misrouted to the body), so a rewrite drops that one leading
//! byte. This is a benign encoding normalization applied to every consumer
//! of the parser, not a fix-specific defect -- workspace `.md` files are
//! UTF-8 without a BOM by convention.

use std::path::{Path, PathBuf};

use frontmatter::{
    propose_fix, propose_skeleton, render, validate, FrontmatterValue, Profile, RawFields,
};
use serde::Serialize;

use crate::cli::FixArgs;
use crate::lint::{passes_scope, repo_relative_posix, resolve_scope};
use crate::scan::ScannedFile;

/// A [`frontmatter::Violation`], reshaped for serialization -- mirrors
/// `lint::LintViolation`'s own reshaping of the same upstream type.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FixViolation {
    pub code: String,
    pub field: String,
    pub message: String,
}

impl From<&frontmatter::Violation> for FixViolation {
    fn from(v: &frontmatter::Violation) -> Self {
        Self {
            code: v.code.clone(),
            field: v.field.clone(),
            message: v.message.clone(),
        }
    }
}

/// Which write this file's repair is -- purely a report label; both cases
/// share one write formula (see this module's doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixAction {
    /// No usable frontmatter existed (absent or empty); a full skeleton is
    /// inserted at the top of the file.
    Skeleton,
    /// A frontmatter block existed and is replaced in place.
    BlockUpdate,
    /// The file's frontmatter didn't parse at all; skipped, never written.
    Unfixable,
}

/// One scanned file's fix verdict -- the shared payload for both JSON and
/// terse output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FixFileResult {
    pub path: String,
    pub file_class: Option<String>,
    pub action: FixAction,
    /// Violations found before repair, when this file reached `validate`
    /// (empty for `Skeleton`/`Unfixable`, which never reach it).
    pub violations_before: Vec<FixViolation>,
    /// Whether the rendered replacement differs from the file's current
    /// content -- what `--apply` would write, or what dry-run reports as
    /// the proposed change.
    pub changed: bool,
    /// Required fields this pass could only stub with a placeholder --
    /// Tier-2's boundary: a human or a later gated model turn still owes
    /// these real content. Never fabricated by this pass.
    pub human_authored_fields: Vec<String>,
    /// Whether this file's class required re-nesting under `workspace:`
    /// before the write (see [`is_externally_schemad_class`]).
    pub workspace_nested: bool,
    /// Set only for `Unfixable`: why this file's frontmatter couldn't be
    /// parsed at all.
    pub unfixable_reason: Option<String>,
}

/// The scan-wide fix rollup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct FixRollup {
    pub scanned: u64,
    pub changed: u64,
    pub unfixable: u64,
    /// Number of files with at least one field still needing authored
    /// content after this pass.
    pub human_authored_pending: u64,
}

/// The full `fix` result -- every scoped file's verdict, path-sorted, plus
/// the rollup folded across them, and whether this run wrote anything.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FixResult {
    pub apply: bool,
    pub files: Vec<FixFileResult>,
    pub rollup: FixRollup,
}

/// A `fix::run` failure -- distinct from a per-file verdict (which is
/// always reported, never fails the run) because these are configuration
/// or usage defects that make the whole invocation meaningless.
#[derive(Debug)]
pub enum FixError {
    /// A `--set` value's syntax wasn't `FIELD=VALUE`.
    InvalidSetSyntax(String),
    /// `--set` was given but scope resolved to other than exactly one file
    /// -- an authored value is per-file, never a blanket rewrite.
    SetRequiresSingleFile(usize),
    /// A write (`--apply`) failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for FixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FixError::InvalidSetSyntax(entry) => {
                write!(f, "invalid --set value {entry:?}, expected FIELD=VALUE")
            }
            FixError::SetRequiresSingleFile(count) => write!(
                f,
                "--set requires scope to resolve to exactly one file, resolved to {count}"
            ),
            FixError::Io { path, source } => {
                write!(f, "failed to write {}: {source}", path.display())
            }
        }
    }
}

/// The file classes the external Claude Code frontmatter schema owns.
///
/// For these classes, Claude Code's own schema owns the file's top-level
/// keys (`name`/`description`); every field this workspace's profile
/// manages (`id`/`tags`/`links`/`updated`, and any other pack-declared
/// field) must nest under a `workspace:` mapping instead of colliding with
/// that schema at the top level -- see `CLAUDE.md`'s Frontmatter section
/// ("For files governed by an external schema ... these fields move under
/// a `workspace:` key"). `"rule"` is included even though the bundled pack
/// declares no `file_class` glob rule for it today (only `skill`/`agent`
/// currently classify): the workspace's own convention names Skills,
/// Agents, and Rules as one group, so this set is future-proofed against a
/// pack that adds the rule glob later, rather than hardcoded to only what
/// happens to be reachable right now.
fn is_externally_schemad_class(file_class: &str) -> bool {
    matches!(file_class, "skill" | "agent" | "rule")
}

/// The profile-managed fields the external Claude Code schema owns at the
/// top level -- kept top-level rather than nested under `workspace:`. Every
/// OTHER profile-managed field (`id`/`tags`/`links`/`updated`, ...) nests;
/// every NON-managed passthrough field -- the external schema's own
/// top-level keys such as `tools`/`model`/`allowed-tools`/`argument-hint` --
/// stays at the top level untouched. See [`nest_workspace_fields`].
const EXTERNAL_SCHEMA_TOP_LEVEL_FIELDS: [&str; 2] = ["name", "description"];

/// Nests the workspace-managed fields (`managed_fields` minus the
/// Claude-Code-owned [`EXTERNAL_SCHEMA_TOP_LEVEL_FIELDS`]) under a trailing
/// `workspace:` mapping, leaving name/description and every non-managed
/// passthrough field (the external schema's own top-level keys, e.g.
/// `tools`/`model`/`allowed-tools`) exactly where they are -- burying those
/// under `workspace:` would silently disable an agent's tool allowlist or
/// model pin. `managed_fields` is the resolved profile's required-field set,
/// so the machine/human boundary here tracks the schema, not a hardcoded
/// list. `render`'s `Mapping` case renders the nested block recursively, so
/// no change to the pure library's renderer is needed for this to work.
fn nest_workspace_fields(fields: &RawFields, managed_fields: &[&str]) -> RawFields {
    let mut top = Vec::new();
    let mut nested = Vec::new();
    for (key, value) in fields.iter() {
        let nest_here =
            managed_fields.contains(&key) && !EXTERNAL_SCHEMA_TOP_LEVEL_FIELDS.contains(&key);
        if nest_here {
            nested.push((key.to_string(), value.clone()));
        } else {
            top.push((key.to_string(), value.clone()));
        }
    }
    top.push((
        "workspace".to_string(),
        FrontmatterValue::Mapping(RawFields::from_ordered_pairs(nested)),
    ));
    RawFields::from_ordered_pairs(top)
}

/// Parses `--set FIELD=VALUE` entries into `(field, value)` pairs.
fn parse_overrides(raw: &[String]) -> Result<Vec<(String, String)>, FixError> {
    raw.iter()
        .map(|entry| {
            entry
                .split_once('=')
                .map(|(field, value)| (field.to_string(), value.to_string()))
                .ok_or_else(|| FixError::InvalidSetSyntax(entry.clone()))
        })
        .collect()
}

/// Merges `overrides` into `fields`, replacing an existing field's value or
/// appending a new one. A comma-separated value fills a field that's
/// already a `Sequence` (or is named `tags`/`links`, this schema's two list
/// fields, when absent); every other field is filled as a plain `Scalar`.
/// This is the Tier-2 boundary's only mechanism for real content to reach
/// a rendered file: the caller supplies the value, this function never
/// invents one.
fn apply_overrides(fields: &RawFields, overrides: &[(String, String)]) -> RawFields {
    let mut pairs: Vec<(String, FrontmatterValue)> = fields
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    for (field, value) in overrides {
        let is_sequence_field = matches!(fields.get(field), Some(FrontmatterValue::Sequence(_)))
            || field == "tags"
            || field == "links";
        let new_value = if is_sequence_field {
            FrontmatterValue::Sequence(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        } else {
            FrontmatterValue::Scalar(value.clone())
        };
        match pairs.iter_mut().find(|(k, _)| k == field) {
            Some(entry) => entry.1 = new_value,
            None => pairs.push((field.clone(), new_value)),
        }
    }
    RawFields::from_ordered_pairs(pairs)
}

/// Runs `args`' fix over `scanned` (the pre-computed corpus from
/// [`crate::scan::scan`]), against `profile`, stamping every repaired
/// `updated:` with `now`. Writes to disk only when `args.apply` is set;
/// otherwise this is a pure report -- see this module's doc comment for
/// the dispatch/write-formula/malformed-frontmatter rules.
pub fn run(
    scanned: &[ScannedFile],
    args: &FixArgs,
    repo_root: &Path,
    profile: &Profile,
    now: &str,
) -> Result<FixResult, FixError> {
    let overrides = parse_overrides(&args.set)?;

    let scope = resolve_scope(Some(&args.scope), repo_root);
    let mut in_scope: Vec<&ScannedFile> = scanned
        .iter()
        .filter(|file| passes_scope(&file.path, scope.as_deref()))
        .collect();
    in_scope.sort_by(|a, b| a.path.cmp(&b.path));

    if !overrides.is_empty() && in_scope.len() != 1 {
        return Err(FixError::SetRequiresSingleFile(in_scope.len()));
    }

    let mut rollup = FixRollup::default();
    let mut files = Vec::new();

    for file in in_scope {
        let path = file.path.to_string_lossy().into_owned();
        rollup.scanned += 1;

        let Ok(parsed) = &file.parsed else {
            let reason = file
                .parsed
                .as_ref()
                .err()
                .map(std::string::ToString::to_string);
            rollup.unfixable += 1;
            files.push(FixFileResult {
                path,
                file_class: None,
                action: FixAction::Unfixable,
                violations_before: Vec::new(),
                changed: false,
                human_authored_fields: Vec::new(),
                workspace_nested: false,
                unfixable_reason: reason,
            });
            continue;
        };

        let rel_path = repo_relative_posix(&file.path, repo_root);
        let (mut proposal, action, violations_before) = if parsed.raw_fields.is_empty() {
            (
                propose_skeleton(&rel_path, profile, now),
                FixAction::Skeleton,
                Vec::new(),
            )
        } else {
            let entry = validate(parsed, &rel_path, profile);
            let violations = entry.violations.iter().map(FixViolation::from).collect();
            (
                propose_fix(parsed, &rel_path, profile, now),
                FixAction::BlockUpdate,
                violations,
            )
        };

        if !overrides.is_empty() {
            proposal.fields = apply_overrides(&proposal.fields, &overrides);
            proposal
                .human_authored_fields
                .retain(|field| !overrides.iter().any(|(f, _)| f == field));
        }

        let file_class = proposal.file_class.clone();
        let human_authored_fields = proposal.human_authored_fields.clone();
        let workspace_nested = is_externally_schemad_class(&file_class);
        let fields = if workspace_nested {
            let managed_fields: Vec<&str> = profile.required_fields().map(|(f, _)| f).collect();
            nest_workspace_fields(&proposal.fields, &managed_fields)
        } else {
            proposal.fields
        };

        let new_content = format!("{}{}", render(&fields), parsed.body_text);
        // Changed unless the file reads back byte-identical to the render;
        // an unreadable file is treated as changed (a fix would rewrite it).
        let changed =
            !std::fs::read_to_string(&file.path).is_ok_and(|original| original == new_content);

        if args.apply && changed {
            std::fs::write(&file.path, &new_content).map_err(|source| FixError::Io {
                path: file.path.clone(),
                source,
            })?;
        }

        if changed {
            rollup.changed += 1;
        }
        if !human_authored_fields.is_empty() {
            rollup.human_authored_pending += 1;
        }

        files.push(FixFileResult {
            path,
            file_class: Some(file_class),
            action,
            violations_before,
            changed,
            human_authored_fields,
            workspace_nested,
            unfixable_reason: None,
        });
    }

    Ok(FixResult {
        apply: args.apply,
        files,
        rollup,
    })
}

// Retained as a test-only shape/determinism helper: the shipped output is
// the clikit `ResultRecord` on stdout, never a terse human summary.
#[cfg(test)]
fn render_terse(result: &FixResult) -> String {
    let mode = if result.apply { "apply" } else { "dry-run" };
    let mut lines: Vec<String> = result
        .files
        .iter()
        .map(|file| {
            let action = match file.action {
                FixAction::Skeleton => "skeleton",
                FixAction::BlockUpdate => "block-update",
                FixAction::Unfixable => "unfixable",
            };
            let changed = if file.changed { "changed" } else { "unchanged" };
            let human = if file.human_authored_fields.is_empty() {
                String::new()
            } else {
                format!(" human_authored=[{}]", file.human_authored_fields.join(","))
            };
            format!("{}: {action} {changed}{human}", file.path)
        })
        .collect();
    let rollup = &result.rollup;
    lines.push(format!(
        "{mode}: scanned={} changed={} unfixable={} human_authored_pending={}",
        rollup.scanned, rollup.changed, rollup.unfixable, rollup.human_authored_pending
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

    fn args(scope: &str) -> FixArgs {
        FixArgs {
            scope: PathBuf::from(scope),
            apply: false,
            dir: Vec::new(),
            set: Vec::new(),
        }
    }

    fn scan_fixture(root: &Path) -> Vec<ScannedFile> {
        let cache_base = TempDir::new().unwrap();
        let mut cache = FreshnessCache::open_in(cache_base.path(), root);
        crate::scan::scan(&[root.to_path_buf()], &mut cache)
    }

    const NOW: &str = "2026-07-13T00:00:00Z";

    #[test]
    fn dry_run_writes_nothing_and_reports_the_proposed_change() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "---\nname: \"x\"\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args("doc.md"), root.path(), &profile, NOW).unwrap();

        assert!(!result.apply);
        assert_eq!(result.files.len(), 1);
        assert!(result.files[0].changed);
        assert!(!result.files[0].human_authored_fields.is_empty());

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert_eq!(
            on_disk, "---\nname: \"x\"\n---\nbody\n",
            "dry-run must not write"
        );
    }

    #[test]
    fn apply_makes_a_fixable_violation_file_valid_and_stamps_now() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "---\nname: \"x\"\ndescription: \"d\"\nid: \"a:b:c\"\ntags:\n  - type:knowledge\n  - status:complete\n  - privacy:internal\n  - owner:datadog\n  - topic:t\nlinks: []\nupdated: 2020-01-01T00:00:00Z\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        assert!(result.files[0].changed);

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(on_disk.contains(&format!("updated: {NOW}")));
        assert!(on_disk.ends_with("body\n"), "body must be preserved");

        let reparsed = frontmatter::parse(&on_disk).unwrap();
        let entry = validate(&reparsed, "doc.md", &profile);
        assert!(entry.is_valid, "{:?}", entry.violations);
    }

    #[test]
    fn skeleton_insert_preserves_the_whole_original_body_byte_for_byte() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "# just a heading\nno frontmatter\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        assert_eq!(result.files[0].action, FixAction::Skeleton);

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(on_disk.ends_with("# just a heading\nno frontmatter\n"));
    }

    #[test]
    fn block_update_preserves_the_body_after_the_replaced_block_byte_for_byte() {
        let root = TempDir::new().unwrap();
        let body = "the exact original body\nwith more than one line\n";
        write_md(
            root.path(),
            "doc.md",
            &format!("---\nname: \"x\"\n---\n{body}"),
        );
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        assert_eq!(result.files[0].action, FixAction::BlockUpdate);

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(on_disk.ends_with(body));
    }

    #[test]
    fn skill_class_nests_managed_fields_under_workspace_flat_context_does_not() {
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            ".claude/skills/my-skill/SKILL.md",
            "---\nname: \"x\"\n---\nbody\n",
        );
        write_md(root.path(), "doc.md", "---\nname: \"x\"\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args(".");
        a.apply = true;
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();

        let skill = result
            .files
            .iter()
            .find(|f| f.path.contains("SKILL.md"))
            .unwrap();
        assert!(skill.workspace_nested);
        let context = result
            .files
            .iter()
            .find(|f| f.path.ends_with("doc.md"))
            .unwrap();
        assert!(!context.workspace_nested);

        let skill_on_disk =
            fs::read_to_string(root.path().join(".claude/skills/my-skill/SKILL.md")).unwrap();
        assert!(skill_on_disk.contains("workspace:"));
        assert!(skill_on_disk.contains("  id:"));
        let context_on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(!context_on_disk.contains("workspace:"));

        // Round-trips valid either way.
        let reparsed_skill = frontmatter::parse(&skill_on_disk).unwrap();
        assert!(
            validate(
                &reparsed_skill,
                ".claude/skills/my-skill/SKILL.md",
                &profile
            )
            .is_valid
        );
    }

    #[test]
    fn externally_schemad_class_keeps_claude_code_top_level_fields_out_of_workspace() {
        // A real agent file carries Claude Code's own top-level fields
        // (tools, model) alongside name/description, with only the
        // workspace-managed fields under workspace:. fix must nest ONLY the
        // managed fields -- burying tools/model under workspace: would
        // silently disable the agent's tool allowlist and model pin.
        let root = TempDir::new().unwrap();
        write_md(
            root.path(),
            ".claude/agents/reviewer.md",
            "---\nname: \"reviewer\"\ndescription: \"d\"\ntools: \"Read, Bash\"\nmodel: \"claude-sonnet-5\"\nworkspace:\n  id: \"agent:x:reviewer\"\n  tags:\n    - type:agent\n    - topic:tooling\n    - status:complete\n    - privacy:internal\n    - owner:datadog\n  links: []\n  updated: 2020-01-01T00:00:00Z\n---\nbody\n",
        );
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args(".claude/agents/reviewer.md");
        a.apply = true;
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        assert_eq!(result.files[0].file_class, Some("agent".to_string()));
        assert!(result.files[0].workspace_nested);

        let on_disk = fs::read_to_string(root.path().join(".claude/agents/reviewer.md")).unwrap();
        let reparsed = frontmatter::parse(&on_disk).unwrap();
        assert_eq!(
            reparsed.raw_fields.get("tools"),
            Some(&FrontmatterValue::Scalar("Read, Bash".to_string())),
            "tools must stay a top-level Claude Code field: {on_disk}"
        );
        assert!(
            reparsed.raw_fields.contains_key("model"),
            "model must stay top-level: {on_disk}"
        );
        let Some(FrontmatterValue::Mapping(ws)) = reparsed.raw_fields.get("workspace") else {
            panic!("expected a workspace mapping: {on_disk}");
        };
        assert!(ws.contains_key("id"));
        assert!(ws.contains_key("updated"));
        assert!(
            !reparsed.raw_fields.contains_key("id"),
            "managed fields must not also appear at top level: {on_disk}"
        );

        // Round-trips valid.
        assert!(
            validate(&reparsed, ".claude/agents/reviewer.md", &profile).is_valid,
            "{:?}",
            validate(&reparsed, ".claude/agents/reviewer.md", &profile).violations
        );
    }

    #[test]
    fn human_authored_field_is_stubbed_and_reported_never_fabricated() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "# no frontmatter\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args("doc.md"), root.path(), &profile, NOW).unwrap();
        let file = &result.files[0];
        assert!(file.human_authored_fields.contains(&"name".to_string()));
        assert!(file
            .human_authored_fields
            .contains(&"description".to_string()));
    }

    #[test]
    fn set_overrides_an_authored_field_and_clears_it_from_pending() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "# no frontmatter\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        a.set = vec!["name=Authored Name".to_string()];
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        let file = &result.files[0];
        assert!(!file.human_authored_fields.contains(&"name".to_string()));

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(on_disk.contains("name: \"Authored Name\""));
    }

    #[test]
    fn set_rejects_a_multi_file_scope() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "a.md", "# no frontmatter\n");
        write_md(root.path(), "b.md", "# no frontmatter\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args(".");
        a.set = vec!["name=x".to_string()];
        let err = run(&scanned, &a, root.path(), &profile, NOW).unwrap_err();
        assert!(matches!(err, FixError::SetRequiresSingleFile(2)));
    }

    #[test]
    fn malformed_frontmatter_is_unfixable_and_left_untouched() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "bad.md", "---\ntags: [unclosed\n---\nbody\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("bad.md");
        a.apply = true;
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        assert_eq!(result.files[0].action, FixAction::Unfixable);
        assert!(result.files[0].unfixable_reason.is_some());

        let on_disk = fs::read_to_string(root.path().join("bad.md")).unwrap();
        assert_eq!(on_disk, "---\ntags: [unclosed\n---\nbody\n");
    }

    // -- SDET hardening (M4.P3.T2 test-engineer pass) -----------------------

    #[test]
    fn crlf_body_after_block_update_is_preserved_byte_for_byte() {
        let root = TempDir::new().unwrap();
        let body = "line one\r\nline two\r\n";
        write_md(
            root.path(),
            "doc.md",
            &format!("---\r\nname: \"x\"\r\n---\r\n{body}"),
        );
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        assert_eq!(result.files[0].action, FixAction::BlockUpdate);

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(
            on_disk.ends_with(body),
            "CRLF body must survive untouched; got: {on_disk:?}"
        );
    }

    #[test]
    fn crlf_skeleton_insert_preserves_the_original_body_byte_for_byte() {
        let root = TempDir::new().unwrap();
        let body = "# heading\r\nno frontmatter\r\n";
        write_md(root.path(), "doc.md", body);
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        assert_eq!(result.files[0].action, FixAction::Skeleton);

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(
            on_disk.ends_with(body),
            "CRLF body must survive untouched; got: {on_disk:?}"
        );
    }

    #[test]
    fn body_with_no_trailing_newline_is_preserved_exactly() {
        let root = TempDir::new().unwrap();
        let body = "no trailing newline at all";
        write_md(
            root.path(),
            "doc.md",
            &format!("---\nname: \"x\"\n---\n{body}"),
        );
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        run(&scanned, &a, root.path(), &profile, NOW).unwrap();

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(
            on_disk.ends_with(body) && !on_disk.ends_with(&format!("{body}\n")),
            "body without a trailing newline must not gain one; got: {on_disk:?}"
        );
    }

    // FINDING (report to QR): a leading UTF-8 BOM is silently dropped from
    // the written output, in both the no-frontmatter (skeleton) and
    // has-frontmatter (block-update) cases. `frontmatter::parse` strips the
    // BOM from its *local* `input` binding before splitting into lines
    // (parse.rs L70), and both `body_only`'s `body_text` (no-frontmatter
    // case) and the post-closing-delimiter `body_text` (has-frontmatter
    // case) are sliced from that already-stripped string -- the BOM byte
    // never reaches `ParsedFrontmatter` at all, so `fix::run`'s write
    // formula (`render(&fields) + parsed.body_text`) has no BOM to
    // preserve. This is a genuine byte-for-byte violation of this module's
    // own "body preserved byte-for-byte" doc-comment claim, scoped
    // precisely to this one leading byte. Whether it's acceptable (BOM is
    // presentation metadata, not content) is a product call for the QR;
    // this test pins the CURRENT (lossy) behavior so a future change to it
    // is a deliberate, visible decision rather than a silent regression.
    #[test]
    fn leading_bom_is_dropped_from_output_no_frontmatter_case() {
        let root = TempDir::new().unwrap();
        let body = "\u{feff}# heading\nno frontmatter\n";
        write_md(root.path(), "doc.md", body);
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        run(&scanned, &a, root.path(), &profile, NOW).unwrap();

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(
            !on_disk.starts_with('\u{feff}'),
            "documenting current (lossy) behavior: BOM is dropped, not preserved"
        );
        assert!(on_disk.contains("# heading\nno frontmatter\n"));
    }

    #[test]
    fn leading_bom_is_dropped_from_output_block_update_case() {
        let root = TempDir::new().unwrap();
        let body = "kept body\n";
        write_md(
            root.path(),
            "doc.md",
            &format!("\u{feff}---\nname: \"x\"\n---\n{body}"),
        );
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        run(&scanned, &a, root.path(), &profile, NOW).unwrap();

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(
            !on_disk.starts_with('\u{feff}'),
            "documenting current (lossy) behavior: BOM is dropped, not preserved"
        );
    }

    #[test]
    fn malformed_set_syntax_is_a_clean_error_not_a_panic() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "# no frontmatter\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.set = vec!["no_equals_sign_here".to_string()];
        let err = run(&scanned, &a, root.path(), &profile, NOW).unwrap_err();
        assert!(matches!(err, FixError::InvalidSetSyntax(ref s) if s == "no_equals_sign_here"));
    }

    #[test]
    fn set_on_a_list_field_comma_splits_and_trims_each_item() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "# no frontmatter\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;
        a.set = vec!["tags=type:knowledge, status:complete ,topic:t".to_string()];
        let result = run(&scanned, &a, root.path(), &profile, NOW).unwrap();
        assert!(!result.files[0]
            .human_authored_fields
            .contains(&"tags".to_string()));

        let on_disk = fs::read_to_string(root.path().join("doc.md")).unwrap();
        assert!(on_disk.contains("  - type:knowledge\n"));
        assert!(on_disk.contains("  - status:complete\n"));
        assert!(on_disk.contains("  - topic:t\n"));
        // No leading/trailing whitespace leaked into a rendered item.
        assert!(!on_disk.contains("  -  status:complete"));
    }

    #[test]
    fn fix_is_idempotent_across_two_successive_apply_runs() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "# no frontmatter\n");

        let profile = crate::test_support::default_profile_for_tests();
        let mut a = args("doc.md");
        a.apply = true;

        let scanned1 = scan_fixture(root.path());
        run(&scanned1, &a, root.path(), &profile, NOW).unwrap();
        let after_first = fs::read_to_string(root.path().join("doc.md")).unwrap();

        let scanned2 = scan_fixture(root.path());
        let result2 = run(&scanned2, &a, root.path(), &profile, NOW).unwrap();
        let after_second = fs::read_to_string(root.path().join("doc.md")).unwrap();

        assert_eq!(
            after_first, after_second,
            "re-running fix against its own already-fixed output must be a no-op"
        );
        assert!(
            !result2.files[0].changed,
            "second run must report unchanged once already schema-valid"
        );
    }

    #[test]
    fn repeated_dry_runs_on_a_fixed_tree_produce_byte_identical_reports() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "a.md", "# no frontmatter\n");
        write_md(root.path(), "b.md", "---\nname: \"x\"\n---\nbody\n");

        let profile = crate::test_support::default_profile_for_tests();
        let a1 = args(".");

        let scanned1 = scan_fixture(root.path());
        let first = run(&scanned1, &a1, root.path(), &profile, NOW).unwrap();
        let scanned2 = scan_fixture(root.path());
        let second = run(&scanned2, &a1, root.path(), &profile, NOW).unwrap();

        assert_eq!(render_terse(&first), render_terse(&second));
        assert_eq!(first, second);
    }

    #[test]
    fn multi_file_scope_reports_files_in_stable_sorted_path_order() {
        let root = TempDir::new().unwrap();
        write_md(root.path(), "z.md", "# no frontmatter\n");
        write_md(root.path(), "a.md", "# no frontmatter\n");
        write_md(root.path(), "m.md", "# no frontmatter\n");
        let scanned = scan_fixture(root.path());

        let profile = crate::test_support::default_profile_for_tests();
        let result = run(&scanned, &args("."), root.path(), &profile, NOW).unwrap();
        let paths: Vec<&str> = result
            .files
            .iter()
            .map(|f| f.path.rsplit('/').next().unwrap())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(
            paths, sorted,
            "file order must be stable path order: {paths:?}"
        );
    }

    #[test]
    fn a_pack_change_flips_which_fields_are_human_authored() {
        // The machine/human boundary is read entirely off the merged
        // profile, not hardcoded here -- a bundled-vs-custom profile with a
        // different authorship split for the same field must flip which
        // fields this pass reports as pending, at the proposal level.
        let root = TempDir::new().unwrap();
        write_md(root.path(), "doc.md", "# no frontmatter\n");
        let scanned = scan_fixture(root.path());

        let bundled = crate::test_support::default_profile_for_tests();
        let bundled_result = run(&scanned, &args("doc.md"), root.path(), &bundled, NOW).unwrap();
        assert!(bundled_result.files[0]
            .human_authored_fields
            .contains(&"updated".to_string())
            .then_some(())
            .is_none());
        assert!(bundled_result.files[0]
            .human_authored_fields
            .contains(&"name".to_string()));
    }
}
