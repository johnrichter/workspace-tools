//! Sentinel -> merged-[`Profile`] resolution: turns a repo's `navigator.toml`
//! into the [`Profile`] a subcommand validates/searches/finds/fixes against.
//!
//! # Modes
//! Every declared `extensions` entry names either a bundled pack (resolved
//! by [`frontmatter::embedded_pack_json`]) or a committed-file path read
//! relative to `repo_root`. [`ResolveMode`] decides what happens when one of
//! those committed-path reads fails:
//! - [`ResolveMode::Gate`] -- fail closed. Any per-pack load failure is a
//!   hard error. Used by `lint`: a gate/CI check must honor every declared
//!   pack or refuse to validate at all, never silently validate against a
//!   smaller pack set than the repo declared.
//! - [`ResolveMode::Discovery`] -- fail soft. A per-pack load failure is
//!   skipped, not fatal; [`resolve`] keeps going with the packs that DID
//!   load and reports what got skipped via [`Resolution::degraded`]. Used
//!   by `search`/`find`/`fix`, where one broken extension pack shouldn't
//!   block every other command from working over the packs that loaded
//!   fine.
//!
//! # Neutral core-only default
//! A repo that never committed a `navigator.toml` ([`Adoption::NotAdopted`])
//! and a repo that committed one with an empty `extensions` list both
//! resolve to the same neutral floor: [`frontmatter::Profile::core_only`],
//! an empty-vocabulary `Profile` that lints/searches/finds/fixes without
//! error -- there is simply no namespace vocabulary to check anything
//! against. This binary never constructs a `Profile` from a bundled,
//! privileged pack; the only packs it ever hands the library are ones a
//! repo explicitly declared, resolved at call time via
//! [`frontmatter::Profile::from_packs`].
//!
//! # Failure taxonomy
//! - A per-pack LOAD failure (committed path missing, unreadable, not a
//!   file, or content that isn't valid JSON): [`ResolveError::PackLoad`] in
//!   Gate mode; skipped + degraded in Discovery mode. The variant carries a
//!   [`PackLoadKind`] splitting an environmental I/O fault from a content/
//!   config defect; the target's `main` maps every pack-load failure to one
//!   `PreconditionUnmet` exit code regardless of kind, so the distinction is
//!   retained for diagnostics and tests, not for exit-code selection.
//! - A [`frontmatter::ProfileError`] from [`frontmatter::Profile::from_packs`]
//!   (version skew, meta-schema violation, post-merge integrity violation,
//!   ...): [`ResolveError::Profile`] in BOTH modes -- a fundamentally
//!   incompatible or corrupt pack set, never a transient load miss to
//!   degrade past.
//! - `[schema].profile` naming a core this build doesn't support:
//!   [`ResolveError::UnsupportedCoreProfile`] in BOTH modes -- there is no
//!   registry to resolve a different core against.
//! - Zero loadable packs is NOT an error: an empty declared `extensions`
//!   resolves to core-only unconditionally, and in Discovery mode a set
//!   where every declared entry failed to load ALSO resolves to core-only,
//!   folding every skip into [`Resolution::degraded`] rather than refusing
//!   to produce a `Profile` at all. Gate mode still hard-errors on the
//!   first per-pack load failure via [`ResolveError::PackLoad`] -- it never
//!   reaches an all-skipped state to begin with.
//!
//! [`frontmatter::MergeWarning`]s are informational in either mode: render
//! them via [`render_warnings`] unless suppressed by the sentinel's
//! `schema.suppress_merge_warnings` or the caller's own
//! `--quiet-schema-warnings` flag (combine both into one `suppress` bool
//! before calling). Suppression never reaches a hard error or the degraded
//! notice -- a caller must render [`Resolution::degraded`] unconditionally.
//!
//! `lint` always resolves in [`ResolveMode::Gate`] (a gate must honor every
//! declared pack or refuse outright); `search`/`find`/`fix` resolve in
//! [`ResolveMode::Discovery`], folding [`Resolution::degraded`] into their
//! own top-level degraded notice alongside any nonconformant hit.

use std::fmt;
use std::path::Path;

use frontmatter::{MergeWarning, Profile, ProfileError};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::sentinel::{self, Adoption, Sentinel, SentinelError};

/// Which failure-handling policy [`resolve`] applies to a per-pack LOAD
/// failure -- see the module doc's "Modes" section. Never changes how a
/// [`frontmatter::ProfileError`] or an unsupported core is handled: those
/// are hard errors under both variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    /// Fail closed: a per-pack load failure is a hard error.
    Gate,
    /// Fail soft: skip a broken pack, degrade, keep going.
    Discovery,
}

/// A successfully resolved profile, plus what the caller must do with it.
#[derive(Debug)]
pub struct Resolution {
    /// The merged profile every subcommand validates/searches/finds
    /// against.
    pub profile: Profile,
    /// The resolved `exempt` vocabulary across every loaded pack layer --
    /// see [`ResolvedExempt`]. `frontmatter::Profile` keeps its own copy of
    /// this private, so `lint`'s missing-frontmatter short circuit (which
    /// never reaches `frontmatter::validate`, the only place that consults
    /// it) reads this one instead.
    pub exempt: ResolvedExempt,
    /// [`frontmatter::Profile::from_packs`]' own merge warnings, in its
    /// documented deterministic order. Render via [`render_warnings`].
    pub warnings: Vec<MergeWarning>,
    /// `Some(reason)` naming which declared pack(s) were skipped and why,
    /// set only in [`ResolveMode::Discovery`] when at least one declared
    /// pack failed to load. Always `None` in [`ResolveMode::Gate`] (a load
    /// failure there is a hard error, never a degrade) and in Discovery
    /// mode when every declared pack loaded cleanly.
    pub degraded: Option<String>,
    /// The sentinel's own `schema.suppress_merge_warnings` (`false` for
    /// `NotAdopted`, which never has a sentinel to read it from). A caller
    /// ORs this with its own `--quiet-schema-warnings` flag to get the
    /// effective suppression passed to [`render_warnings`].
    pub suppress_merge_warnings: bool,
}

/// Whether a per-pack load failure was an environmental I/O fault or a
/// content/config defect. Lets the caller map [`ResolveError::PackLoad`] to
/// the same exit code its sentinel-file sibling already uses: a failed read
/// syscall is `IO_ERROR` whether it hit the sentinel or a declared pack;
/// unusable content is `USAGE_ERROR` either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackLoadKind {
    /// A filesystem access failed (path unreadable/absent, read error) --
    /// an environmental fault, not a defect in the declared pack itself.
    Io,
    /// The path resolved but is unusable as a pack: not a regular file, or
    /// not valid JSON -- a defect in what the sentinel declared.
    Content,
}

/// Why [`resolve`] could not produce a [`Resolution`] at all -- every
/// variant here is a hard error in EITHER [`ResolveMode`]. Zero loadable
/// packs is deliberately NOT one of these variants -- see the module doc's
/// "Neutral core-only default" section.
#[derive(Debug)]
pub enum ResolveError {
    /// The sentinel itself failed to load -- see [`SentinelError`].
    Sentinel(SentinelError),
    /// `[schema].profile` names a core this build doesn't support. This
    /// build supports only the embedded core's own declared version --
    /// there is no registry to resolve a different core against.
    UnsupportedCoreProfile { declared: String, supported: String },
    /// A committed-path pack failed to load, in [`ResolveMode::Gate`]
    /// ([`ResolveMode::Discovery`] skips this instead of erroring -- see
    /// [`Resolution::degraded`]). `kind` splits an environmental I/O fault
    /// from a content/config defect -- see [`PackLoadKind`]. It is classified
    /// and asserted by tests but not consumed by the exit-code mapping, which
    /// treats every pack-load failure as one `PreconditionUnmet`.
    PackLoad {
        entry: String,
        #[allow(dead_code)]
        kind: PackLoadKind,
        detail: String,
    },
    /// [`frontmatter::Profile::from_packs`] rejected the resolved pack set
    /// -- never degradable, in either mode.
    Profile(ProfileError),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sentinel(err) => write!(f, "{err}"),
            // M-DIST.P4.T1's skew guard: `declared` is the sentinel's
            // `[schema].profile`, `supported` is this binary's embedded
            // core version (via the linked frontmatter crate) -- naming
            // both, plus both ways to resolve the skew, so an operator
            // never has to go source-diving to unblock a fail-closed run.
            Self::UnsupportedCoreProfile {
                declared,
                supported,
            } => write!(
                f,
                "skew: navigator.toml declares schema.profile = \"{declared}\", but this \
                 navigator build's embedded core is \"{supported}\". Resolve by either (1) \
                 bumping navigator_version in navigator.toml to a released build whose \
                 embedded core is \"{declared}\", or (2) setting schema.profile = \"{supported}\" \
                 in navigator.toml to match this build."
            ),
            Self::PackLoad { entry, detail, .. } => {
                write!(f, "failed to load extension pack \"{entry}\": {detail}")
            }
            Self::Profile(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sentinel(err) => Some(err),
            Self::Profile(err) => Some(err),
            Self::UnsupportedCoreProfile { .. } | Self::PackLoad { .. } => None,
        }
    }
}

/// Resolves `repo_root`'s merged [`Profile`], per this module's doc.
pub fn resolve(repo_root: &Path, mode: ResolveMode) -> Result<Resolution, ResolveError> {
    let sentinel = match sentinel::load(repo_root).map_err(ResolveError::Sentinel)? {
        // No sentinel at all -- the neutral core-only floor, never an
        // error (SC2/SC8). This binary never reaches into a bundled,
        // privileged pack to decide a not-adopted repo's behavior.
        Adoption::NotAdopted => return Ok(core_only_resolution()),
        Adoption::Adopted(sentinel) => sentinel,
    };

    let core_json = frontmatter::embedded_core_json();
    let core_version = declared_core_version(core_json);
    if sentinel.schema.profile != core_version {
        return Err(ResolveError::UnsupportedCoreProfile {
            declared: sentinel.schema.profile.clone(),
            supported: core_version.clone(),
        });
    }

    let (loaded, skipped) = load_packs(&sentinel, repo_root, mode)?;

    if loaded.is_empty() {
        // An adopted repo with nothing left to load -- an empty declared
        // `extensions`, or (Discovery mode only; Gate already hard-errored
        // above) every declared entry skipped -- is the same neutral
        // core-only floor the not-adopted path resolves to, carrying
        // forward this sentinel's own suppression setting and naming
        // whatever got skipped.
        let mut resolution = core_only_resolution();
        resolution.suppress_merge_warnings = sentinel.schema.suppress_merge_warnings;
        resolution.degraded =
            (!skipped.is_empty()).then(|| format!("skipped pack(s): {}", skipped.join("; ")));
        return Ok(resolution);
    }

    let pack_refs: Vec<&str> = loaded.iter().map(String::as_str).collect();
    let (profile, warnings) =
        Profile::from_packs(core_json, &pack_refs).map_err(ResolveError::Profile)?;

    // `loaded`'s JSON already parsed once inside `Profile::from_packs` (glob
    // syntax included) -- `merge_exempt` re-reading it here can't hit an
    // error that call didn't already reject.
    let exempt = merge_exempt(&loaded);

    let degraded =
        (!skipped.is_empty()).then(|| format!("skipped pack(s): {}", skipped.join("; ")));

    Ok(Resolution {
        profile,
        exempt,
        warnings,
        degraded,
        suppress_merge_warnings: sentinel.schema.suppress_merge_warnings,
    })
}

/// The neutral core-only floor: zero packs, [`Profile::core_only`], no
/// exempt vocabulary, no warnings, no suppression. Every caller that
/// reaches this -- not-adopted, or an adopted repo with nothing left to
/// load -- gets the exact same empty-vocabulary `Profile`; a caller who
/// needs a different suppression/degraded value overrides those two fields
/// on the returned [`Resolution`] itself.
fn core_only_resolution() -> Resolution {
    let core_json = frontmatter::embedded_core_json();
    let profile = Profile::core_only(core_json)
        .expect("the embedded core JSON is this crate's own committed schema file");
    Resolution {
        profile,
        exempt: merge_exempt(&[]),
        warnings: Vec::new(),
        degraded: None,
        suppress_merge_warnings: false,
    }
}

/// The resolved `exempt` vocabulary (`filenames`/`dir_components`/
/// `path_globs`) a file matching any of which skips frontmatter enforcement
/// entirely -- the same three-way match `frontmatter::validate`'s own
/// (private) `is_exempt` uses. Computed independently of
/// [`frontmatter::Profile`] (whose merged `exempt` set is private to that
/// crate) because `lint`'s missing-frontmatter short circuit -- a file with
/// no `---`-fenced block at all, or an empty one -- never reaches
/// `frontmatter::validate`, the only place that consults it; without this,
/// a repo-owner exempting a deliberately frontmatter-free file (a plain
/// README, a test fixture) via the pack's `exempt` block would still see it
/// counted as `missing_frontmatter` and fail the gate.
#[derive(Debug)]
pub struct ResolvedExempt {
    filenames: Vec<String>,
    dir_components: Vec<String>,
    path_globs: GlobSet,
}

impl ResolvedExempt {
    /// True iff `rel_path` (repo-root-relative, forward-slash) matches
    /// `filenames` by basename, `dir_components` by any path segment, or
    /// `path_globs` by full-path glob.
    pub fn is_exempt(&self, rel_path: &str) -> bool {
        let basename = rel_path.rsplit('/').next().unwrap_or(rel_path);
        if self.filenames.iter().any(|f| f == basename) {
            return true;
        }
        if rel_path
            .split('/')
            .any(|part| self.dir_components.iter().any(|d| d == part))
        {
            return true;
        }
        self.path_globs.is_match(rel_path)
    }
}

/// Unions `filenames`/`dir_components`/`path_globs` across every pack in
/// `pack_jsons`, in declared order -- additive merge only, no
/// removal-directive support (unlike `frontmatter::Profile::from_packs`'
/// own cascade). Exact for today's single-layer, no-removals sentinel; a
/// future multi-layer pack set relying on an exempt removal would need this
/// to grow that same handling.
///
/// **Tracked debt (spike Finding 3):** `frontmatter::Profile` keeps its
/// merged `exempt` set `pub(crate)` -- there is no public accessor this
/// function could call instead. Until the library adds one, this ad-hoc
/// second parse stays the only way `lint`'s missing-frontmatter short
/// circuit (see [`Resolution::exempt`]'s doc) can read the same vocabulary
/// `frontmatter::validate`'s own exempt gate consults. Retiring this in
/// favor of a library accessor is a future `rust/frontmatter` API addition,
/// out of this task's scope (R-PUB is merged; this task consumes it
/// read-only).
fn merge_exempt(pack_jsons: &[String]) -> ResolvedExempt {
    let mut filenames = Vec::new();
    let mut dir_components = Vec::new();
    let mut glob_builder = GlobSetBuilder::new();

    for text in pack_jsons {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            continue;
        };
        let Some(exempt) = value.get("exempt") else {
            continue;
        };
        let strings = |key: &str| -> Vec<String> {
            exempt
                .get(key)
                .and_then(serde_json::Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| entry.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        filenames.extend(strings("filenames"));
        dir_components.extend(strings("dir_components"));
        for pattern in strings("path_globs") {
            // Every caller passes pack JSON `Profile::from_packs` already
            // accepted, which compiles this exact glob set itself -- a
            // failure here would mean that call should have failed too.
            let glob = Glob::new(&pattern).expect(
                "path_globs pattern already validated by frontmatter::Profile construction",
            );
            glob_builder.add(glob);
        }
    }

    let path_globs = glob_builder
        .build()
        .expect("path_globs already validated by frontmatter::Profile construction");

    ResolvedExempt {
        filenames,
        dir_components,
        path_globs,
    }
}

/// Loads every entry in `sentinel.extensions`, in declared order. Returns
/// the loaded pack JSON texts (in that same order) plus a human-readable
/// detail string per skipped entry. In [`ResolveMode::Gate`], the first
/// per-pack load failure returns immediately as [`ResolveError::PackLoad`]
/// instead of being collected into `skipped`.
fn load_packs(
    sentinel: &Sentinel,
    repo_root: &Path,
    mode: ResolveMode,
) -> Result<(Vec<String>, Vec<String>), ResolveError> {
    let mut loaded = Vec::new();
    let mut skipped = Vec::new();

    for entry in &sentinel.extensions {
        match load_pack(entry, repo_root) {
            Ok(text) => loaded.push(text),
            Err((kind, detail)) => match mode {
                ResolveMode::Gate => {
                    return Err(ResolveError::PackLoad {
                        entry: entry.clone(),
                        kind,
                        detail,
                    });
                }
                ResolveMode::Discovery => skipped.push(format!("{entry} ({detail})")),
            },
        }
    }

    Ok((loaded, skipped))
}

/// Resolves one `extensions` entry to its pack JSON text: a named bundle
/// via [`frontmatter::embedded_pack_json`], else a `repo_root`-relative
/// committed-file path. On any per-pack defect returns its
/// [`PackLoadKind`] plus a human-readable detail string (missing/unreadable
/// -> [`PackLoadKind::Io`]; not a file / not valid JSON ->
/// [`PackLoadKind::Content`]) -- never panics, since [`ResolveMode::Discovery`]
/// must be able to skip past exactly these defects.
fn load_pack(entry: &str, repo_root: &Path) -> Result<String, (PackLoadKind, String)> {
    if let Some(bundled) = frontmatter::embedded_pack_json(entry) {
        return Ok(bundled.to_string());
    }

    let path = repo_root.join(entry);
    let io_err = |err| (PackLoadKind::Io, format!("{}: {err}", path.display()));

    let metadata = std::fs::metadata(&path).map_err(io_err)?;
    if !metadata.is_file() {
        return Err((
            PackLoadKind::Content,
            format!("{}: not a file", path.display()),
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(io_err)?;
    if let Err(err) = serde_json::from_str::<serde_json::Value>(&text) {
        return Err((
            PackLoadKind::Content,
            format!("{}: not valid JSON: {err}", path.display()),
        ));
    }
    Ok(text)
}

/// The embedded core's own declared `version` (e.g. `"core@2.0.0"`), read from
/// its JSON text directly rather than through `frontmatter::Core` (private
/// to that crate) -- the same source of truth `embedded_pack_json` reads
/// its own resolver key from.
///
/// # Panics
/// Never: the embedded core JSON is `frontmatter`'s own committed schema
/// file, whose shape that crate's tests pin.
fn declared_core_version(core_json: &str) -> String {
    let value: serde_json::Value =
        serde_json::from_str(core_json).expect("embedded core JSON must parse");
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .expect("embedded core JSON must declare a string 'version'")
        .to_string()
}

/// One human-readable line per `warning`, in the profile's deterministic
/// merge-warning order, or an empty list when `suppress` is true. `suppress`
/// is the caller's already-combined
/// `sentinel.schema.suppress_merge_warnings || --quiet-schema-warnings` --
/// this function has no opinion on where that bool came from, and applies it
/// only to warnings, never to a hard error or a degraded notice. The caller
/// logs each returned line through logkit (the CLI's one log stream);
/// nothing here writes to stderr directly.
#[must_use]
pub fn render_warnings(warnings: &[MergeWarning], suppress: bool) -> Vec<String> {
    if suppress {
        return Vec::new();
    }
    warnings.iter().map(format_warning).collect()
}

/// `command`'s one material-effect note, or `None` when `suppress` is true:
/// a single line naming that every semantic decision this run made (which
/// namespaces exist, their types, description caps, conformance rules) came
/// from the merged profile resolved for this repo, not from `command`'s own
/// hardcoded defaults. Distinct from [`render_warnings`] (specific pack-merge
/// overrides/removals) and from a caller's own degraded-state notice (a
/// partial resolution, never suppressible). Suppressed by the exact same
/// `suppress` bool `render_warnings` takes, so one flag controls both. The
/// caller logs the returned line through logkit.
#[must_use]
pub fn render_material_effect_note(command: &str, suppress: bool) -> Option<String> {
    if suppress {
        return None;
    }
    Some(format!(
        "{command}: results reflect the merged profile resolved for this repo \
         (schema.profile plus every loaded extensions entry) -- pass --quiet-schema-warnings \
         to suppress this note."
    ))
}

/// One human-readable line for a [`MergeWarning`]. `MergeWarning`/
/// [`Dimension`] carry no `Display` of their own (the crate that defines
/// them has no rendering need), so this is navigator's own presentation --
/// `{dimension:?}` reads fine as-is (`RequiredField`, `DescriptionCap`, ...).
fn format_warning(warning: &MergeWarning) -> String {
    match warning {
        MergeWarning::Override {
            dimension,
            key,
            from_layer,
            to_layer,
            base_layer,
        } => format!(
            "{} schema-pack override: {dimension:?} \"{key}\" from \"{from_layer}\" replaced by \"{to_layer}\"",
            severity(*base_layer),
        ),
        MergeWarning::Removal {
            dimension,
            key,
            removing_layer,
            removed_from_layer,
            base_layer,
        } => match removed_from_layer {
            Some(from_layer) => format!(
                "{} schema-pack removal: {dimension:?} \"{key}\" (defined by \"{from_layer}\") removed by \"{removing_layer}\"",
                severity(*base_layer),
            ),
            None => format!(
                "{} schema-pack removal: {dimension:?} \"{key}\" removed by \"{removing_layer}\" (no layer had defined it)",
                severity(*base_layer),
            ),
        },
    }
}

/// WARN when the overridden/removed definition came from the base
/// vocabulary pack (`pack_jsons[0]`), INFO otherwise -- mirrors
/// [`frontmatter::Profile::from_packs`]' own severity rule.
fn severity(base_layer: bool) -> &'static str {
    if base_layer {
        "WARN"
    } else {
        "INFO"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frontmatter::Dimension;
    use std::fs;
    use tempfile::TempDir;

    fn write_sentinel(root: &Path, extensions_toml: &str) {
        fs::write(
            root.join("navigator.toml"),
            format!(
                "sentinel_version = 2\n{extensions_toml}\n\n[schema]\nprofile = \"core@2.0.0\"\n"
            ),
        )
        .unwrap();
    }

    /// A minimal, structurally valid extension pack extending `core@2.0.0`,
    /// with `description_caps.context` fixed to `value` so two of these at
    /// different values can exercise override-order/warning tests.
    fn minimal_pack(version: &str, context_cap: u32) -> String {
        format!(
            r#"{{
                "kind": "extension-pack",
                "version": "{version}",
                "extends": "core@2.0.0",
                "required_fields": [{{"field": "name", "authorship": "human_authored"}}],
                "description_caps": {{"context": {context_cap}}},
                "file_class": {{"default": "context", "rules": []}},
                "namespaces": [{{"name": "type", "cardinality": "singleton"}}],
                "exempt": {{"filenames": [], "dir_components": [], "path_globs": []}}
            }}"#
        )
    }

    /// A structurally valid pack that declares an `extends` version this
    /// build's embedded core can never match -- exercises the version-skew
    /// guard on a committed pack (as opposed to the sentinel-level skew
    /// guard, which `skew_guard_*` below covers separately).
    fn skewed_pack() -> String {
        r#"{
            "kind": "extension-pack",
            "version": "skewed@1",
            "extends": "core@0.0.0",
            "required_fields": [],
            "description_caps": {"context": 100},
            "file_class": {"default": "context", "rules": []},
            "namespaces": [{"name": "type", "cardinality": "singleton"}],
            "exempt": {"filenames": [], "dir_components": [], "path_globs": []}
        }"#
        .to_string()
    }

    // -- NotAdopted ---------------------------------------------------------

    #[test]
    fn not_adopted_resolves_to_core_only_not_degraded() {
        let root = TempDir::new().unwrap();
        let resolution = resolve(root.path(), ResolveMode::Gate).unwrap();
        assert!(resolution.warnings.is_empty());
        assert!(resolution.degraded.is_none());
        assert!(!resolution.suppress_merge_warnings);
        // No sentinel means no pack layered on the core -- the profile
        // carries no vocabulary at all.
        assert_eq!(resolution.profile.description_cap("context"), None);
    }

    // -- Ordered resolution: bundle + committed path, declared order -------

    #[test]
    fn named_bundle_and_committed_path_resolve_in_declared_order() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("committed.json"),
            minimal_pack("committed@1", 999),
        )
        .unwrap();
        write_sentinel(
            root.path(),
            r#"extensions = ["default@1.0.0", "committed.json"]"#,
        );

        let resolution = resolve(root.path(), ResolveMode::Gate).unwrap();
        assert!(resolution.degraded.is_none());
        // committed.json is declared AFTER the bundle, so its
        // description_caps.context value must win (last-definer-wins).
        assert_eq!(resolution.profile.description_cap("context"), Some(999));

        let override_warning = resolution
            .warnings
            .iter()
            .find(|w| matches!(w, MergeWarning::Override { dimension: Dimension::DescriptionCap, key, .. } if key == "context"))
            .expect("expected a description_caps override warning");
        if let MergeWarning::Override {
            from_layer,
            to_layer,
            ..
        } = override_warning
        {
            assert_eq!(from_layer, "default@1.0.0");
            assert_eq!(to_layer, "committed@1");
        }
    }

    #[test]
    fn reversed_declared_order_flips_which_value_wins() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("committed.json"),
            minimal_pack("committed@1", 999),
        )
        .unwrap();
        write_sentinel(
            root.path(),
            r#"extensions = ["committed.json", "default@1.0.0"]"#,
        );

        let resolution = resolve(root.path(), ResolveMode::Gate).unwrap();
        // default@1.0.0 is now declared LAST, so its own description_caps.context
        // (350, per the bundled pack) wins over committed.json's 999.
        assert_eq!(resolution.profile.description_cap("context"), Some(350));
    }

    #[test]
    fn three_layer_merge_declared_order_drives_the_outcome_not_pack_identity() {
        // Three layers: the named bundle plus two committed-path packs, each
        // with a distinct description_caps.context so the winning value
        // pins exactly which layer merged last -- not merely "some override
        // happened".
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("a.json"), minimal_pack("a@1", 111)).unwrap();
        fs::write(root.path().join("b.json"), minimal_pack("b@1", 222)).unwrap();
        write_sentinel(
            root.path(),
            r#"extensions = ["default@1.0.0", "a.json", "b.json"]"#,
        );

        let resolution = resolve(root.path(), ResolveMode::Gate).unwrap();
        assert!(resolution.degraded.is_none());
        // b.json is declared last, so its value wins over both a.json (222
        // beats 111) and the bundle (222 beats 350).
        assert_eq!(resolution.profile.description_cap("context"), Some(222));
    }

    #[test]
    fn three_layer_merge_reordering_the_middle_and_last_pack_flips_the_winner() {
        // Same three layers and same set of values as the test above, but
        // with a.json and b.json swapped in the declared order -- proves
        // the merge outcome tracks DECLARED ORDER, not which pack happens
        // to hold which value.
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("a.json"), minimal_pack("a@1", 111)).unwrap();
        fs::write(root.path().join("b.json"), minimal_pack("b@1", 222)).unwrap();
        write_sentinel(
            root.path(),
            r#"extensions = ["default@1.0.0", "b.json", "a.json"]"#,
        );

        let resolution = resolve(root.path(), ResolveMode::Gate).unwrap();
        assert!(resolution.degraded.is_none());
        // a.json is now declared last, so 111 wins instead of 222.
        assert_eq!(resolution.profile.description_cap("context"), Some(111));
    }

    // -- Per-pack load-failure classes --------------------------------------

    #[test]
    fn gate_mode_hard_errors_on_missing_committed_path() {
        let root = TempDir::new().unwrap();
        write_sentinel(root.path(), r#"extensions = "no-such-pack.json""#);

        let err = resolve(root.path(), ResolveMode::Gate).unwrap_err();
        // A missing committed path is a failed read syscall -> Io, so the
        // caller maps it to the same exit code as an unreadable sentinel.
        assert!(matches!(
            err,
            ResolveError::PackLoad {
                kind: PackLoadKind::Io,
                ..
            }
        ));
        assert!(err.to_string().contains("no-such-pack.json"));
    }

    #[test]
    fn gate_mode_hard_errors_on_committed_path_that_is_a_directory() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("a-directory")).unwrap();
        write_sentinel(root.path(), r#"extensions = "a-directory""#);

        let err = resolve(root.path(), ResolveMode::Gate).unwrap_err();
        // The path exists but is a directory -- a config defect, not I/O.
        assert!(matches!(
            err,
            ResolveError::PackLoad {
                kind: PackLoadKind::Content,
                ..
            }
        ));
        assert!(err.to_string().contains("not a file"));
    }

    #[test]
    fn gate_mode_hard_errors_on_committed_path_with_invalid_json() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("broken.json"), "not valid json").unwrap();
        write_sentinel(root.path(), r#"extensions = "broken.json""#);

        let err = resolve(root.path(), ResolveMode::Gate).unwrap_err();
        // The file read fine; its bytes just aren't JSON -- a content defect.
        assert!(matches!(
            err,
            ResolveError::PackLoad {
                kind: PackLoadKind::Content,
                ..
            }
        ));
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn discovery_mode_skips_missing_committed_path_and_degrades() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            r#"extensions = ["default@1.0.0", "no-such-pack.json"]"#,
        );

        let resolution = resolve(root.path(), ResolveMode::Discovery).unwrap();
        let reason = resolution.degraded.expect("expected a degraded reason");
        assert!(reason.contains("no-such-pack.json"));
        // Built from the pack that DID load.
        assert_eq!(resolution.profile.description_cap("context"), Some(350));
    }

    #[test]
    fn discovery_mode_skips_directory_and_non_json_committed_paths() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("a-directory")).unwrap();
        fs::write(root.path().join("broken.json"), "not valid json").unwrap();
        write_sentinel(
            root.path(),
            r#"extensions = ["default@1.0.0", "a-directory", "broken.json"]"#,
        );

        let resolution = resolve(root.path(), ResolveMode::Discovery).unwrap();
        let reason = resolution.degraded.expect("expected a degraded reason");
        assert!(reason.contains("a-directory"));
        assert!(reason.contains("broken.json"));
    }

    #[test]
    fn discovery_mode_degraded_reason_names_every_skip_when_reasons_differ() {
        // Two packs skipped for genuinely DIFFERENT failure classes (a
        // missing path vs. invalid JSON content) in the same run -- the
        // degraded reason must name both entries, each with its own detail,
        // not just report "something failed" or only the first miss.
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("broken.json"), "not valid json").unwrap();
        write_sentinel(
            root.path(),
            r#"extensions = ["default@1.0.0", "no-such-pack.json", "broken.json"]"#,
        );

        let resolution = resolve(root.path(), ResolveMode::Discovery).unwrap();
        let reason = resolution.degraded.expect("expected a degraded reason");
        assert!(
            reason.contains("no-such-pack.json"),
            "missing-path entry not named: {reason}"
        );
        assert!(
            reason.contains("broken.json"),
            "invalid-JSON entry not named: {reason}"
        );
        assert!(
            reason.contains("not valid JSON"),
            "invalid-JSON detail not present: {reason}"
        );
        // Built from the one pack that DID load.
        assert_eq!(resolution.profile.description_cap("context"), Some(350));
    }

    // -- Zero loadable packs resolve to core-only, not an error -------------

    #[test]
    fn empty_extensions_resolves_to_core_only_in_both_modes() {
        let root = TempDir::new().unwrap();
        write_sentinel(root.path(), "extensions = []");

        for mode in [ResolveMode::Gate, ResolveMode::Discovery] {
            let resolution = resolve(root.path(), mode).unwrap();
            assert!(resolution.degraded.is_none());
            assert_eq!(resolution.profile.description_cap("context"), None);
        }
    }

    #[test]
    fn discovery_mode_with_every_pack_broken_degrades_to_core_only() {
        let root = TempDir::new().unwrap();
        write_sentinel(
            root.path(),
            r#"extensions = ["no-such-pack.json", "also-missing.json"]"#,
        );

        let resolution = resolve(root.path(), ResolveMode::Discovery).unwrap();
        let reason = resolution.degraded.expect("expected a degraded reason");
        assert!(reason.contains("no-such-pack.json"));
        assert!(reason.contains("also-missing.json"));
        // No pack survived to load, so the profile carries no vocabulary.
        assert_eq!(resolution.profile.description_cap("context"), None);
    }

    // -- from_packs hard errors surface in both modes -----------------------

    #[test]
    fn version_skew_in_a_committed_pack_is_a_hard_error_in_both_modes() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("skewed.json"), skewed_pack()).unwrap();
        write_sentinel(root.path(), r#"extensions = "skewed.json""#);

        for mode in [ResolveMode::Gate, ResolveMode::Discovery] {
            let err = resolve(root.path(), mode).unwrap_err();
            match &err {
                ResolveError::Profile(ProfileError::VersionSkew { .. }) => {}
                other => panic!("expected Profile(VersionSkew), got {other:?}"),
            }
        }
    }

    // -- Unsupported [schema].profile ---------------------------------------

    #[test]
    fn unsupported_core_profile_is_a_hard_error_in_both_modes() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("navigator.toml"),
            "sentinel_version = 2\nextensions = \"default@1.0.0\"\n\n[schema]\nprofile = \"core@99\"\n",
        )
        .unwrap();

        for mode in [ResolveMode::Gate, ResolveMode::Discovery] {
            let err = resolve(root.path(), mode).unwrap_err();
            match &err {
                ResolveError::UnsupportedCoreProfile { declared, .. } => {
                    assert_eq!(declared, "core@99");
                }
                other => panic!("expected UnsupportedCoreProfile, got {other:?}"),
            }
        }
    }

    // -- M-DIST.P4.T1 skew guard ----------------------------------------------
    // sentinel `[schema].profile` vs this binary's embedded core (via the
    // linked frontmatter crate) -- `unsupported_core_profile_is_a_hard_error_
    // in_both_modes` above already covers the mismatch shape/error-variant;
    // these two pin the guard's two required outcomes explicitly: identical
    // versions never false-positive, and a mismatch's message is actionable.

    #[test]
    fn skew_guard_passes_when_sentinel_profile_matches_embedded_core() {
        let root = TempDir::new().unwrap();
        write_sentinel(root.path(), r#"extensions = "default@1.0.0""#);

        // write_sentinel declares `schema.profile = "core@2.0.0"`, matching this
        // build's embedded core exactly -- resolution must succeed, not
        // false-positive a skew.
        let resolution = resolve(root.path(), ResolveMode::Gate).unwrap();
        assert!(resolution.degraded.is_none());
    }

    #[test]
    fn skew_guard_fails_closed_with_actionable_guidance_on_mismatch() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("navigator.toml"),
            "sentinel_version = 2\nextensions = \"default@1.0.0\"\n\n[schema]\nprofile = \"core@1\"\n",
        )
        .unwrap();

        let err = resolve(root.path(), ResolveMode::Gate).unwrap_err();
        let message = err.to_string();
        // Names both versions...
        assert!(
            message.contains("core@1"),
            "missing declared version: {message}"
        );
        assert!(
            message.contains("core@2.0.0"),
            "missing embedded version: {message}"
        );
        // ...and both ways to resolve the skew.
        assert!(
            message.contains("navigator_version"),
            "missing the bump-the-binary resolution path: {message}"
        );
        assert!(
            message.contains("schema.profile"),
            "missing the bump-the-sentinel resolution path: {message}"
        );
    }

    // -- Sentinel load failure propagates ------------------------------------

    #[test]
    fn malformed_sentinel_is_a_clear_error_not_a_silent_fallback() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("navigator.toml"), "not valid [ toml").unwrap();

        let err = resolve(root.path(), ResolveMode::Gate).unwrap_err();
        assert!(matches!(err, ResolveError::Sentinel(_)));
    }

    // -- Merge-warning suppression --------------------------------------------

    fn resolution_with_one_override(root: &Path) -> Resolution {
        fs::write(
            root.join("committed.json"),
            minimal_pack("committed@1", 999),
        )
        .unwrap();
        write_sentinel(root, r#"extensions = ["default@1.0.0", "committed.json"]"#);
        resolve(root, ResolveMode::Gate).unwrap()
    }

    #[test]
    fn sentinel_suppress_flag_suppresses_rendering() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join("committed.json"),
            minimal_pack("committed@1", 999),
        )
        .unwrap();
        fs::write(
            root.path().join("navigator.toml"),
            "sentinel_version = 2\nextensions = [\"default@1.0.0\", \"committed.json\"]\n\n\
             [schema]\nprofile = \"core@2.0.0\"\nsuppress_merge_warnings = true\n",
        )
        .unwrap();

        let resolution = resolve(root.path(), ResolveMode::Gate).unwrap();
        assert!(!resolution.warnings.is_empty());
        assert!(resolution.suppress_merge_warnings);
    }

    #[test]
    fn no_suppression_leaves_warnings_present_for_the_caller_to_render() {
        let root = TempDir::new().unwrap();
        let resolution = resolution_with_one_override(root.path());
        assert!(!resolution.warnings.is_empty());
        assert!(!resolution.suppress_merge_warnings);
        // render_warnings itself has no observable return value (it writes
        // to stderr); this pins the render-decision inputs a caller
        // combines (sentinel flag OR --quiet-schema-warnings) rather than
        // asserting on captured stderr text.
    }

    // -- merge_exempt / ResolvedExempt ---------------------------------------

    fn pack_with_exempt(
        filenames: &[&str],
        dir_components: &[&str],
        path_globs: &[&str],
    ) -> String {
        let quote_join = |items: &[&str]| {
            items
                .iter()
                .map(|s| format!("{s:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            r#"{{
                "kind": "extension-pack",
                "version": "exempt-test@1",
                "extends": "core@2.0.0",
                "required_fields": [],
                "description_caps": {{"context": 500}},
                "file_class": {{"default": "context", "rules": []}},
                "namespaces": [{{"name": "type", "cardinality": "optional"}}],
                "exempt": {{
                    "filenames": [{}],
                    "dir_components": [{}],
                    "path_globs": [{}]
                }}
            }}"#,
            quote_join(filenames),
            quote_join(dir_components),
            quote_join(path_globs),
        )
    }

    #[test]
    fn merge_exempt_unions_filenames_dir_components_and_path_globs_across_pack_layers() {
        let layer_a = pack_with_exempt(&["README.md"], &[], &[]);
        let layer_b = pack_with_exempt(&[], &["vendor"], &["docs/*.generated.md"]);
        let exempt = merge_exempt(&[layer_a, layer_b]);

        assert!(
            exempt.is_exempt("some/dir/README.md"),
            "filenames from layer A"
        );
        assert!(
            exempt.is_exempt("vendor/anything.md"),
            "dir_components from layer B"
        );
        assert!(
            exempt.is_exempt("docs/x.generated.md"),
            "path_globs from layer B"
        );
        assert!(
            !exempt.is_exempt("docs/x.md"),
            "glob must not over-match a near-miss"
        );
    }

    #[test]
    fn merge_exempt_is_non_vacuous_a_file_not_covered_by_any_layer_is_not_exempt() {
        let exempt = merge_exempt(&[pack_with_exempt(&["README.md"], &[], &[])]);
        assert!(!exempt.is_exempt("plugins/navigator/NOTAREADME.md"));
        assert!(!exempt.is_exempt("anything/else.md"));
    }

    #[test]
    fn is_exempt_filenames_match_is_exact_basename_case_and_suffix_sensitive() {
        let exempt = merge_exempt(&[pack_with_exempt(&["README.md"], &[], &[])]);
        assert!(exempt.is_exempt("a/b/README.md"));
        assert!(
            !exempt.is_exempt("a/b/readme.md"),
            "lowercase must not match"
        );
        assert!(
            !exempt.is_exempt("a/b/README.markdown"),
            "different suffix must not match"
        );
        assert!(
            !exempt.is_exempt("a/b/xREADME.md"),
            "must match the whole basename, not a substring"
        );
    }

    #[test]
    fn is_exempt_dir_components_match_any_path_segment_not_a_substring_of_one() {
        let exempt = merge_exempt(&[pack_with_exempt(&[], &["testdata"], &[])]);
        assert!(exempt.is_exempt("plugins/x/testdata/fixture.md"));
        assert!(
            exempt.is_exempt("testdata/fixture.md"),
            "leading segment too"
        );
        assert!(
            !exempt.is_exempt("plugins/x/nottestdata/fixture.md"),
            "a component that merely CONTAINS the name must not match"
        );
    }

    #[test]
    fn is_exempt_empty_exempt_vocabulary_exempts_nothing() {
        let exempt = merge_exempt(&[pack_with_exempt(&[], &[], &[])]);
        assert!(!exempt.is_exempt("README.md"));
        assert!(!exempt.is_exempt("anything.md"));
    }

    #[test]
    fn merge_exempt_skips_unparseable_or_exempt_less_layers_without_panicking() {
        let exempt = merge_exempt(&[
            "not valid json".to_string(),
            pack_with_exempt(&["README.md"], &[], &[]),
        ]);
        assert!(
            exempt.is_exempt("README.md"),
            "a later valid layer still contributes"
        );
    }
}
