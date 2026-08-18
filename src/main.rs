//! `navigator` -- the CLI entry point.
//!
//! Composes the `frontmatter`, `facetquery` and `bm25` crates (schema
//! parsing, boolean/facet query evaluation, and ranking) with the `clikit`
//! and `logkit` crates (this binary's output contract) into `search`,
//! `find`, `lint` and `fix` -- see `cli.rs`, which owns parsing and
//! `--help`, for each verb's argument surface and worked examples.
//!
//! One invocation: parse args, resolve the runtime knobs (`flag > env >
//! navigator.toml > default`), gather the reachable-set corpus through the
//! out-of-tree freshness cache, run the requested subcommand against the
//! merged frontmatter profile, then write exactly one clikit [`ResultRecord`]
//! to stdout as canonical JSON and exit with its paired code from the closed
//! eleven-member taxonomy. Every subcommand's own narration goes to stderr
//! through the one logkit [`Logger`]; stdout never carries a log line.

mod cache;
mod cli;
mod config;
mod conformance;
mod corpus;
mod filter;
mod find;
mod fix;
mod lint;
mod mdwalk;
mod profile_resolve;
mod querybuild;
mod reachable;
mod scan;
mod search;
mod sentinel;
mod skipset;
#[cfg(test)]
mod test_support;

use std::path::Path;
use std::process::ExitCode;

use clap::Parser;
use clikit::{Diagnostic, ResultRecord, ResultRecordBuilder, Status, Triage};
use facetquery::EvalDiagnostic;
use logkit::{Level, Logger, Sink};
use serde::Serialize;
use serde_json::Value;

use cli::{Cli, Command, FindArgs, FixArgs, LintArgs, SearchArgs};
use profile_resolve::{Resolution, ResolveError, ResolveMode};
use scan::ScannedFile;

/// This CLI's clikit tool name and logkit service name -- one string by the
/// contract's `service_binding` rule. The invoked binary keeps the name
/// `navigator`; the repo, plugin, and env-knob prefix are what the port
/// renamed.
const TOOL: &str = "navigator";

fn main() -> ExitCode {
    let cli = Cli::parse();
    let command_name = cli.command.name();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // Runtime knobs layer flag > WORKSPACE_TOOLS_ env > navigator.toml >
    // default. The sentinel is read once here for the `quiet_schema_warnings`
    // file layer; a sentinel that won't even load is a precondition failure
    // reported through the same contract every other outcome uses (and
    // `profile_resolve` would reject it identically per verb anyway).
    let adoption = match sentinel::load(&cwd) {
        Ok(adoption) => adoption,
        Err(err) => {
            let logger = build_logger(false);
            return finish(&logger, &sentinel_load_failure(command_name, &err));
        }
    };
    let sentinel_ref = match &adoption {
        sentinel::Adoption::Adopted(sentinel) => Some(sentinel),
        sentinel::Adoption::NotAdopted => None,
    };
    let runtime = match config::resolve(sentinel_ref, cli.json, cli.quiet_schema_warnings) {
        Ok(runtime) => runtime,
        Err(err) => {
            let logger = build_logger(false);
            return finish(&logger, &config_failure(command_name, &err));
        }
    };
    let logger = build_logger(runtime.json);

    let scanned = corpus::gather(&cwd, cli.command.dirs(), &logger);
    let quiet = runtime.quiet_schema_warnings;

    let record = match &cli.command {
        Command::Search(args) => run_search(&logger, &scanned, args, &cwd, quiet),
        Command::Find(args) => run_find(&logger, &scanned, args, &cwd, quiet),
        Command::Lint(args) => run_lint(&logger, &scanned, args, &cwd, quiet),
        Command::Fix(args) => run_fix(&logger, &scanned, args, &cwd, quiet),
    };

    finish(&logger, &record)
}

/// Resolves this run's profile in Discovery mode (a broken extension pack
/// degrades the run rather than failing it), narrates the merge
/// warnings/material-effect note, and searches the corpus. A completed
/// search -- even an empty or degraded one -- is [`Status::Success`]; only a
/// malformed query or `--path` glob is a [`Status::Usage`] failure.
fn run_search(
    logger: &Logger,
    scanned: &[ScannedFile],
    args: &SearchArgs,
    cwd: &Path,
    quiet: bool,
) -> ResultRecord {
    let resolution = match profile_resolve::resolve(cwd, ResolveMode::Discovery) {
        Ok(resolution) => resolution,
        Err(err) => return resolve_failure("search", &err),
    };
    narrate(logger, &resolution, "search", quiet);

    match search::run(
        scanned,
        args,
        cwd,
        &resolution.profile,
        resolution.degraded.as_deref(),
    ) {
        Ok((result, diagnostics)) => {
            log_query_diagnostics(logger, "search", &diagnostics);
            success_record("search", &result)
        }
        Err(err) => query_failure("search", &err),
    }
}

/// `find` shares `search`'s Discovery-mode resolution and narration in full,
/// differing only in that `find::run` never ranks a match.
fn run_find(
    logger: &Logger,
    scanned: &[ScannedFile],
    args: &FindArgs,
    cwd: &Path,
    quiet: bool,
) -> ResultRecord {
    let resolution = match profile_resolve::resolve(cwd, ResolveMode::Discovery) {
        Ok(resolution) => resolution,
        Err(err) => return resolve_failure("find", &err),
    };
    narrate(logger, &resolution, "find", quiet);

    match find::run(
        scanned,
        args,
        cwd,
        &resolution.profile,
        resolution.degraded.as_deref(),
    ) {
        Ok((result, diagnostics)) => {
            log_query_diagnostics(logger, "find", &diagnostics);
            success_record("find", &result)
        }
        Err(err) => query_failure("find", &err),
    }
}

/// Resolves this run's profile in Gate mode (a broken declared pack fails
/// the run outright -- a gate must honour every declared pack or refuse to
/// validate) and lints every in-scope file. A completed lint that found at
/// least one invalid or missing-frontmatter file is a [`Status::GateNegative`]
/// -- the gate's answer is "no", not a failure of the invocation -- carrying
/// the full report under `data`; an all-valid scope is [`Status::Success`].
fn run_lint(
    logger: &Logger,
    scanned: &[ScannedFile],
    args: &LintArgs,
    cwd: &Path,
    quiet: bool,
) -> ResultRecord {
    let resolution = match profile_resolve::resolve(cwd, ResolveMode::Gate) {
        Ok(resolution) => resolution,
        Err(err) => return resolve_failure("lint", &err),
    };
    narrate(logger, &resolution, "lint", quiet);

    let mut result = lint::run(scanned, args, cwd, &resolution.profile);
    lint::reclassify_exempt_missing(&mut result, &resolution.exempt, &resolution.profile, cwd);

    if result.rollup.invalid > 0 || result.rollup.missing_frontmatter > 0 {
        let diagnostic = Diagnostic::new(
            "gate_negative.lint.nonconformant",
            one_line(format!(
                "{} invalid, {} missing frontmatter across {} scanned file(s)",
                result.rollup.invalid, result.rollup.missing_frontmatter, result.rollup.scanned
            )),
            Triage::reinvoke([TOOL, "fix", "."])
                .instruction("apply the schema-driven fixes, then re-lint"),
        );
        let builder = with_payload(
            ResultRecord::builder(Status::GateNegative, [TOOL, "lint"]),
            &result,
        )
        .error(diagnostic);
        build_or_internal("lint", builder)
    } else {
        success_record("lint", &result)
    }
}

/// Resolves this run's profile in Gate mode (same rationale as `lint` -- a
/// fix must never repair against the wrong schema) and repairs every
/// in-scope file, stamping `now` into every repaired `updated:`. A completed
/// run -- dry-run or apply -- is [`Status::Success`] regardless of what it
/// found or changed; a bad `--set` is a [`Status::Usage`] failure and a
/// write fault is classified from its OS error kind.
fn run_fix(
    logger: &Logger,
    scanned: &[ScannedFile],
    args: &FixArgs,
    cwd: &Path,
    quiet: bool,
) -> ResultRecord {
    let resolution = match profile_resolve::resolve(cwd, ResolveMode::Gate) {
        Ok(resolution) => resolution,
        Err(err) => return resolve_failure("fix", &err),
    };
    narrate(logger, &resolution, "fix", quiet);

    let now = now_iso8601();
    match fix::run(scanned, args, cwd, &resolution.profile, &now) {
        Ok(result) => success_record("fix", &result),
        Err(err) => fix_failure(&err),
    }
}

/// The current UTC time as an ISO-8601 `updated:`-format timestamp
/// (`YYYY-MM-DDTHH:MM:SSZ`), computed once per invocation and stamped into
/// every file `fix` repairs. The `Z` suffix is literal: `updated:` is always
/// UTC.
fn now_iso8601() -> String {
    let format =
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    time::OffsetDateTime::now_utc()
        .format(&format)
        .expect("a fixed, valid format description always formats a UTC datetime")
}

/// Logs this run's merge warnings and material-effect note through logkit,
/// respecting `quiet` (`--quiet-schema-warnings` or the sentinel's
/// `suppress_merge_warnings`, whichever). The degraded notice folded into a
/// verb's own result is never suppressed here -- that lives in `data`.
fn narrate(logger: &Logger, resolution: &Resolution, verb: &str, quiet: bool) {
    let suppress = resolution.suppress_merge_warnings || quiet;
    for line in profile_resolve::render_warnings(&resolution.warnings, suppress) {
        let _ = logger.warn(line).emit();
    }
    if let Some(note) = profile_resolve::render_material_effect_note(verb, suppress) {
        let _ = logger.info(note).emit();
    }
}

/// Logs each eval-time query diagnostic (an unknown facet, a range against a
/// non-ordered facet) as a warning -- the query still ran, so these never
/// change the record's status.
fn log_query_diagnostics(logger: &Logger, verb: &str, diagnostics: &[EvalDiagnostic]) {
    for diagnostic in diagnostics {
        let _ = logger
            .warn(format!(
                "{verb}: {}",
                querybuild::render_diagnostic(diagnostic)
            ))
            .emit();
    }
}

/// A [`Status::Success`] record whose `data` is the verb's own payload -- the
/// former top-level JSON object, moved under `data` per the RESULTRECORD
/// contract.
fn success_record(verb: &'static str, payload: &impl Serialize) -> ResultRecord {
    build_or_internal(
        verb,
        with_payload(
            ResultRecord::builder(Status::Success, [TOOL, verb]),
            payload,
        ),
    )
}

/// Spreads `payload`'s serialized top-level object across the builder's
/// `data` entries. Every verb result serializes to a JSON object of
/// clikit-legal member keys, so the fallback arm is unreachable in practice.
fn with_payload(builder: ResultRecordBuilder, payload: &impl Serialize) -> ResultRecordBuilder {
    match serde_json::to_value(payload) {
        Ok(Value::Object(map)) => map
            .into_iter()
            .fold(builder, |builder, (key, value)| builder.data(key, value)),
        _ => builder,
    }
}

/// A profile-resolution failure: the schema state this run requires is not
/// in place, so nothing was attempted -- [`Status::PreconditionUnmet`].
fn resolve_failure(verb: &'static str, err: &ResolveError) -> ResultRecord {
    let (code, triage) = match err {
        ResolveError::Sentinel(_) => (
            "precondition_unmet.sentinel.invalid",
            "fix navigator.toml and retry",
        ),
        ResolveError::UnsupportedCoreProfile { .. } => (
            "precondition_unmet.schema.unsupported_core",
            "align navigator.toml's schema.profile with this build, or upgrade navigator",
        ),
        ResolveError::PackLoad { .. } => (
            "precondition_unmet.pack.unloadable",
            "fix or remove the declared extension pack and retry",
        ),
        ResolveError::Profile(_) => (
            "precondition_unmet.schema.invalid_pack",
            "fix the extension pack set navigator.toml declares and retry",
        ),
    };
    failure_record(
        verb,
        Status::PreconditionUnmet,
        code,
        err.to_string(),
        Triage::manual(triage),
    )
}

/// A sentinel that could not be loaded at all (unreadable, malformed, or an
/// unsupported version) -- reported before the logger's rendering mode is
/// even known, so this always builds a human-mode logger's record.
fn sentinel_load_failure(verb: &'static str, err: &sentinel::SentinelError) -> ResultRecord {
    failure_record(
        verb,
        Status::PreconditionUnmet,
        "precondition_unmet.sentinel.invalid",
        err.to_string(),
        Triage::manual("fix navigator.toml and retry"),
    )
}

/// A runtime knob layer (in practice, a `WORKSPACE_TOOLS_*` environment
/// value figment2 can't coerce, e.g. `WORKSPACE_TOOLS_JSON=1`) that doesn't
/// resolve to a valid [`config::RuntimeConfig`]: the invocation's own
/// environment is wrong -- [`Status::Usage`]. Reported before the logger's
/// rendering mode is known, so this always builds a human-mode logger's
/// record, same as [`sentinel_load_failure`].
fn config_failure(verb: &'static str, err: &config::ConfigError) -> ResultRecord {
    failure_record(
        verb,
        Status::Usage,
        "usage.config.invalid_env",
        err.to_string(),
        Triage::manual("fix the WORKSPACE_TOOLS_* environment variable and retry"),
    )
}

/// A malformed positional query or `--path` glob: the invocation itself is
/// wrong -- [`Status::Usage`]. Nothing was scanned.
fn query_failure(verb: &'static str, message: &str) -> ResultRecord {
    failure_record(
        verb,
        Status::Usage,
        "usage.query.invalid",
        message.to_string(),
        Triage::manual("correct the query or --path glob and retry"),
    )
}

/// Maps a `fix::run` failure onto the taxonomy: a bad `--set` is a usage
/// error, a permission-denied write is [`Status::Permission`], and any other
/// write fault is [`Status::Internal`] (an outcome the tool cannot itself
/// classify).
fn fix_failure(err: &fix::FixError) -> ResultRecord {
    match err {
        fix::FixError::InvalidSetSyntax(_) | fix::FixError::SetRequiresSingleFile(_) => {
            failure_record(
                "fix",
                Status::Usage,
                "usage.fix.invalid_set",
                err.to_string(),
                Triage::manual("correct the --set FIELD=VALUE arguments and retry"),
            )
        }
        fix::FixError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            failure_record(
                "fix",
                Status::Permission,
                "permission.fix.write_denied",
                err.to_string(),
                Triage::manual("grant write access to the target file and retry"),
            )
        }
        fix::FixError::Io { .. } => failure_record(
            "fix",
            Status::Internal,
            "internal.fix.write_failed",
            err.to_string(),
            Triage::manual("inspect the reported path and retry"),
        ),
    }
}

/// Builds a failure-class record with one governing diagnostic. The status
/// and code prefix are the caller's to keep in agreement; a mismatch surfaces
/// as an internal record rather than a silent wrong exit code.
fn failure_record(
    verb: &'static str,
    status: Status,
    code: &'static str,
    message: String,
    triage: Triage,
) -> ResultRecord {
    let diagnostic = Diagnostic::new(code, one_line(message), triage);
    build_or_internal(
        verb,
        ResultRecord::builder(status, [TOOL, verb]).error(diagnostic),
    )
}

/// Finishes `builder`, or -- if its own schema validation fails, a navigator
/// defect never a caller problem -- an [`Status::Internal`] record naming it.
fn build_or_internal(verb: &'static str, builder: ResultRecordBuilder) -> ResultRecord {
    builder.build().unwrap_or_else(|err| {
        ResultRecord::builder(Status::Internal, [TOOL, verb])
            .error(Diagnostic::new(
                "internal.clikit.record_build_failed",
                one_line(format!("could not build a result record: {err}")),
                Triage::manual(
                    "this is a navigator defect; file a bug with the command and output",
                ),
            ))
            .build()
            .expect("one correctly-prefixed error always builds an internal record")
    })
}

/// Collapses `message` to one clikit-legal line: control characters become
/// spaces, the result is trimmed and length-bounded, and an empty result
/// falls back to a fixed placeholder (a diagnostic message must be non-empty).
fn one_line(message: impl Into<String>) -> String {
    let cleaned: String = message
        .into()
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || c as u32 == 0x7f {
                ' '
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded: String = trimmed.chars().take(4000).collect();
    if bounded.is_empty() {
        "unspecified error".to_string()
    } else {
        bounded
    }
}

/// Builds the one logkit logger for this run. `json` selects the stderr
/// rendering; the fixed service name always satisfies logkit's schema.
fn build_logger(json: bool) -> Logger {
    let builder = Logger::builder(TOOL)
        .service_version(env!("CARGO_PKG_VERSION"))
        .threshold(Level::Info);
    let builder = if json {
        builder.json_writer(Some(Sink::stderr())).human_writer(None)
    } else {
        builder.json_writer(None).human_writer(Some(Sink::stderr()))
    };
    builder
        .build()
        .expect("the fixed service name 'navigator' always satisfies logkit's schema")
}

/// Writes `record` to stdout as canonical JSON, logs the terminating
/// narration line through logkit, and returns the process's exit code from
/// the clikit taxonomy.
fn finish(logger: &Logger, record: &ResultRecord) -> ExitCode {
    let json = record
        .canonical_json()
        .expect("a built ResultRecord always serializes");
    println!("{json}");
    let _ = clikit::log_terminating(
        logger,
        record,
        format!("{} finished", record.command.join(" ")),
    );
    ExitCode::from(u8::try_from(record.exit_code).unwrap_or(u8::MAX))
}
