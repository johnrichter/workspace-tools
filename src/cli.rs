//! Command-line contract for `navigator`.
//!
//! Defines the four version-1 subcommands and their argument surface.
//! Behavior lives in `main.rs`; this module owns only parsing and `--help`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "navigator",
    version,
    about = "Deterministic frontmatter/tag-aware search and lint over the reachable file set",
    long_about = "navigator finds and validates frontmatter-tagged files across the directories \
        attached to a session (the \"reachable set\"): free-text + tag/type search, exact-path/type \
        lookup, and schema-driven lint/fix of frontmatter blocks."
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of a terse human summary.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress schema-pack merge override/removal warnings for this
    /// invocation (see `navigator.toml`'s `schema.suppress_merge_warnings`
    /// for the persistent, per-repo equivalent -- either one suppresses).
    #[arg(long = "quiet-schema-warnings", global = true)]
    pub quiet_schema_warnings: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Free-text search over the reachable file set, filterable by tag/type.
    #[command(
        after_help = "EXAMPLE:\n    navigator search \"pricing calculator\" --type knowledge --limit 5"
    )]
    Search(SearchArgs),

    /// Exact lookup by type/tag/path, no free-text ranking.
    #[command(after_help = "EXAMPLE:\n    navigator find --type skill --tag topic:apm")]
    Find(FindArgs),

    /// Validate frontmatter against its schema for a file, dir, or the whole reachable set.
    #[command(after_help = "EXAMPLE:\n    navigator lint the-work/deliverables --json")]
    Lint(LintArgs),

    /// Apply schema-driven frontmatter fixes (dry-run by default).
    #[command(after_help = "EXAMPLE:\n    navigator fix agent/identity.md --apply")]
    Fix(FixArgs),
}

impl Command {
    /// The clikit command-path segment this subcommand reports itself as
    /// (`["navigator", <this>]`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Command::Search(_) => "search",
            Command::Find(_) => "find",
            Command::Lint(_) => "lint",
            Command::Fix(_) => "fix",
        }
    }

    /// The `--dir` scope this invocation extends the reachable set with,
    /// common to every subcommand.
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        match self {
            Command::Search(a) => &a.dir,
            Command::Find(a) => &a.dir,
            Command::Lint(a) => &a.dir,
            Command::Fix(a) => &a.dir,
        }
    }
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Free-text query.
    pub query: String,

    /// Filter by tag, repeatable, in `key:value` form (e.g. `topic:apm`).
    #[arg(long = "tag", value_name = "KEY:VALUE")]
    pub tag: Vec<String>,

    /// Filter by frontmatter `type:` value.
    #[arg(long = "type", value_name = "TYPE")]
    pub file_type: Option<String>,

    /// Cap the number of results returned.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Restrict to paths matching this glob.
    #[arg(long, value_name = "GLOB")]
    pub path: Option<String>,

    /// Restrict the search to this directory, repeatable. Defaults to the whole reachable set.
    #[arg(long = "dir", value_name = "PATH")]
    pub dir: Vec<PathBuf>,

    /// Tokenize as whole identifiers (exact-symbol match, e.g. `dd_trace`
    /// stays one token) instead of the default all-case-splitting mode
    /// (which matches `trace` against `dd_trace`/`ddTrace`).
    #[arg(long = "whole-identifier")]
    pub whole_identifier: bool,
}

#[derive(Args, Debug)]
pub struct FindArgs {
    /// Structured facetquery@1 query -- the same syntax `search` accepts
    /// (bareword/phrase terms, `facet:value` predicates, boolean
    /// combinators, ranges). Optional: omit to filter purely by
    /// `--tag`/`--type`/`--path`.
    pub query: Option<String>,

    /// Filter by frontmatter `type:` value.
    #[arg(long = "type", value_name = "TYPE")]
    pub file_type: Option<String>,

    /// Filter by tag, repeatable, in `key:value` form (e.g. `topic:apm`).
    #[arg(long = "tag", value_name = "KEY:VALUE")]
    pub tag: Vec<String>,

    /// Restrict to paths matching this glob.
    #[arg(long, value_name = "GLOB")]
    pub path: Option<String>,

    /// Restrict the lookup to this directory, repeatable. Defaults to the whole reachable set.
    #[arg(long = "dir", value_name = "PATH")]
    pub dir: Vec<PathBuf>,
}

#[derive(Args, Debug)]
pub struct LintArgs {
    /// File or directory to lint. Defaults to the whole reachable set when omitted.
    pub scope: Option<PathBuf>,

    /// Restrict linting to this directory, repeatable. Defaults to the whole reachable set.
    #[arg(long = "dir", value_name = "PATH")]
    pub dir: Vec<PathBuf>,
}

#[derive(Args, Debug)]
pub struct FixArgs {
    /// File or directory to fix.
    pub scope: PathBuf,

    /// Write fixes to disk. Without this flag, fix reports what it would change.
    #[arg(long)]
    pub apply: bool,

    /// Restrict fixing to this directory, repeatable. Defaults to the whole reachable set.
    #[arg(long = "dir", value_name = "PATH")]
    pub dir: Vec<PathBuf>,

    /// Supply an authored value for a human-authored field, repeatable, in
    /// `FIELD=VALUE` form (e.g. `--set description="..."`). Requires `scope`
    /// to resolve to exactly one file -- an authored value is per-file, not
    /// a blanket rewrite. A comma-separated value fills a list field
    /// (`tags`/`links`); anything else fills a scalar field.
    #[arg(long = "set", value_name = "FIELD=VALUE")]
    pub set: Vec<String>,
}
