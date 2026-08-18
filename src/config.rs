//! Runtime settings resolution, `flag > env > file > default`, per
//! SC-STACK. The `navigator.toml` sentinel itself is parsed once, through
//! `sentinel::load`; this module merges the parts of that already-parsed
//! result it cares about back in as one more figment2 layer alongside the
//! `WORKSPACE_TOOLS_*` environment prefix and the CLI flags, so
//! `navigator.toml` is never read from disk twice.
//!
//! Orthogonal to `profile_resolve`: this module resolves the two runtime
//! knobs a run answers to (`json`, `quiet_schema_warnings`), never the
//! frontmatter profile a subcommand validates against.

use figment2::providers::{Env, Serialized};
use figment2::Figment;
use serde::{Deserialize, Serialize};

use crate::sentinel::Sentinel;

/// The environment prefix this CLI's runtime knobs read from. Renamed from
/// the historic `NAVIGATOR_` prefix when the CLI became `workspace-tools`
/// (SC11): the prefix carries the component's name, and the component was
/// renamed.
const ENV_PREFIX: &str = "WORKSPACE_TOOLS_";

/// Settings a repo, the environment, or a flag may each want the final say
/// over -- everything else about a run (which files, which query) is a
/// per-invocation argument, not a layered setting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Renders stderr narration as logkit's machine JSON instead of its
    /// human line. stdout's result record is unaffected either way -- it
    /// is always canonical JSON, per the clikit contract.
    pub json: bool,
    /// Suppresses schema-bundle merge override/removal caveats.
    pub quiet_schema_warnings: bool,
}

/// What `navigator.toml` contributes to [`RuntimeConfig`], as a figment2
/// layer. Only the fields the sentinel schema actually declares appear
/// here -- there is no independent file-level `json` setting.
#[derive(Debug, Serialize)]
struct FromSentinel {
    quiet_schema_warnings: bool,
}

/// What CLI flags contribute, as a figment2 layer. `None` means "not
/// passed" and is omitted from serialization, so an unset flag never
/// overrides a lower layer (`Figment` merge only ever sees the keys a
/// layer actually provides).
#[derive(Debug, Default, Serialize)]
struct FromFlags {
    #[serde(skip_serializing_if = "Option::is_none")]
    json: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quiet_schema_warnings: Option<bool>,
}

/// Resolves the final [`RuntimeConfig`] for this invocation: CLI flags win
/// over `WORKSPACE_TOOLS_*` environment variables, which win over
/// `navigator.toml`, which wins over the built-in default.
#[must_use]
pub fn resolve(
    sentinel: Option<&Sentinel>,
    cli_json: bool,
    cli_quiet_schema_warnings: bool,
) -> RuntimeConfig {
    let mut figment = Figment::new().merge(Serialized::defaults(RuntimeConfig::default()));

    if let Some(sentinel) = sentinel {
        figment = figment.merge(Serialized::defaults(FromSentinel {
            quiet_schema_warnings: sentinel.schema.suppress_merge_warnings,
        }));
    }

    figment = figment.merge(Env::prefixed(ENV_PREFIX));

    figment = figment.merge(Serialized::defaults(FromFlags {
        json: cli_json.then_some(true),
        quiet_schema_warnings: cli_quiet_schema_warnings.then_some(true),
    }));

    figment
        .extract()
        .expect("every layer is a valid RuntimeConfig source")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::{load, Adoption};
    use tempfile::TempDir;

    fn adopted_sentinel(body: &str) -> Sentinel {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("navigator.toml"), body).unwrap();
        match load(root.path()).unwrap() {
            Adoption::Adopted(sentinel) => sentinel,
            Adoption::NotAdopted => panic!("expected an adopted sentinel"),
        }
    }

    #[test]
    fn env_prefix_follows_the_rename() {
        // SC11: the runtime knobs read the WORKSPACE_TOOLS_ prefix, not the
        // historic NAVIGATOR_ one. The prefix's live effect on stderr
        // rendering is asserted end-to-end (with a hermetic child-process
        // env) in tests/cli.rs; this pins the source of truth for it.
        assert_eq!(ENV_PREFIX, "WORKSPACE_TOOLS_");
    }

    #[test]
    fn default_is_human_output_and_warnings_shown() {
        let config = resolve(None, false, false);
        assert!(!config.json);
        assert!(!config.quiet_schema_warnings);
    }

    #[test]
    fn sentinel_can_set_quiet_schema_warnings() {
        let sentinel = adopted_sentinel(
            "sentinel_version = 2\nextensions = []\n\n[schema]\nprofile = \"core@2.0.0\"\nsuppress_merge_warnings = true\n",
        );
        let config = resolve(Some(&sentinel), false, false);
        assert!(config.quiet_schema_warnings);
    }

    #[test]
    fn cli_flag_overrides_everything_below_it() {
        let config = resolve(None, true, false);
        assert!(config.json);
    }
}
