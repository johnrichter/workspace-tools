//! Black-box integration tests against the built `navigator` binary,
//! asserting the fleet output contract: every verb prints exactly one clikit
//! `ResultRecord` on stdout and exits with a code from the closed
//! eleven-member taxonomy (SC5), and the runtime knobs read the
//! `WORKSPACE_TOOLS_` environment prefix (SC11).
//!
//! Each test runs the binary with its cwd set to a throwaway `TempDir` and
//! `HOME`/`XDG_CACHE_HOME` redirected inside it, so the reachable-set walk
//! sees only the test repo and the freshness cache never touches the
//! operator's real one.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

/// The eleven `(status, exit_code)` pairs of the clikit taxonomy.
const TAXONOMY: &[(&str, i64)] = &[
    ("success", 0),
    ("caveats", 10),
    ("gate_negative", 20),
    ("precondition_unmet", 30),
    ("not_found", 40),
    ("conflict", 41),
    ("usage", 50),
    ("transient", 60),
    ("permission", 70),
    ("unsupported", 80),
    ("internal", 90),
];

fn run(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_navigator"))
        .args(args)
        .current_dir(repo)
        .env("HOME", repo.join(".home"))
        .env("XDG_CACHE_HOME", repo.join(".home/.cache"))
        .env_remove("WORKSPACE_TOOLS_JSON")
        .env_remove("WORKSPACE_TOOLS_QUIET_SCHEMA_WARNINGS")
        .output()
        .expect("navigator binary runs")
}

fn write(repo: &Path, rel: &str, contents: &str) {
    let path = repo.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Parses stdout as the single JSON `ResultRecord` and asserts every
/// invariant of the contract that doesn't depend on the specific verb.
fn assert_record(out: &Output, verb: &str) -> Value {
    let text = String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8");
    let record: Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON object ({e}): {text}"));

    assert_eq!(record["schema_version"], Value::from(1));
    assert_eq!(record["command"], serde_json::json!(["navigator", verb]));

    let status = record["status"].as_str().expect("status is a string");
    let exit = record["exit_code"]
        .as_i64()
        .expect("exit_code is an integer");
    assert!(
        TAXONOMY.contains(&(status, exit)),
        "status/exit_code {status:?}/{exit} is not a taxonomy pair"
    );
    let os_exit = out.status.code().expect("process exited with a code");
    assert_eq!(
        i64::from(os_exit),
        exit,
        "process exit code must equal the record's exit_code"
    );
    record
}

#[test]
fn search_emits_a_success_result_record() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "docs/a.md",
        "---\nname: widget-doc\n---\nwidgets and gears\n",
    );

    let out = run(repo.path(), &["search", "widgets"]);
    let record = assert_record(&out, "search");
    assert_eq!(record["status"], "success");
    assert_eq!(record["exit_code"], Value::from(0));
    assert!(
        record["data"]["hits"].is_array(),
        "search data carries hits"
    );
}

#[test]
fn find_emits_a_success_result_record() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "docs/a.md",
        "---\nname: widget-doc\ntags:\n  - type:skill\n---\nbody\n",
    );

    let out = run(repo.path(), &["find", "--type", "skill"]);
    let record = assert_record(&out, "find");
    assert_eq!(record["status"], "success");
    assert!(record["data"]["hits"].is_array());
}

#[test]
fn lint_all_valid_scope_is_success_exit_0() {
    let repo = TempDir::new().unwrap();
    // No navigator.toml: the neutral core-only floor requires no fields, so
    // any parseable frontmatter is valid.
    write(repo.path(), "docs/ok.md", "---\nname: ok\n---\nbody\n");

    let out = run(repo.path(), &["lint", "docs/ok.md"]);
    let record = assert_record(&out, "lint");
    assert_eq!(record["status"], "success");
    assert_eq!(record["exit_code"], Value::from(0));
    assert_eq!(
        record["data"]["rollup"]["missing_frontmatter"],
        Value::from(0)
    );
}

#[test]
fn lint_with_findings_is_gate_negative_exit_20() {
    let repo = TempDir::new().unwrap();
    // A file with no frontmatter block at all -> missing_frontmatter, the
    // gate's "no" answer.
    write(repo.path(), "docs/bare.md", "plain body, no frontmatter\n");

    let out = run(repo.path(), &["lint", "docs/bare.md"]);
    let record = assert_record(&out, "lint");
    assert_eq!(record["status"], "gate_negative");
    assert_eq!(record["exit_code"], Value::from(20));
    assert!(
        record["errors"][0]["code"]
            .as_str()
            .unwrap()
            .starts_with("gate_negative."),
        "a gate-negative record carries a governing gate_negative diagnostic"
    );
    assert!(
        record["data"]["rollup"]["missing_frontmatter"]
            .as_i64()
            .unwrap()
            >= 1
    );
}

#[test]
fn fix_dry_run_is_success_and_writes_nothing() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/bare.md", "plain body, no frontmatter\n");
    let before = std::fs::read(repo.path().join("docs/bare.md")).unwrap();

    let out = run(repo.path(), &["fix", "docs/bare.md"]);
    let record = assert_record(&out, "fix");
    assert_eq!(record["status"], "success");
    assert_eq!(record["data"]["apply"], Value::from(false));

    let after = std::fs::read(repo.path().join("docs/bare.md")).unwrap();
    assert_eq!(before, after, "a dry run must not touch the file");
}

#[test]
fn malformed_query_is_a_usage_failure_exit_50() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/a.md", "---\nname: a\n---\nbody\n");

    let out = run(repo.path(), &["search", "(unclosed"]);
    let record = assert_record(&out, "search");
    assert_eq!(record["status"], "usage");
    assert_eq!(record["exit_code"], Value::from(50));
    assert!(record["errors"][0]["code"]
        .as_str()
        .unwrap()
        .starts_with("usage."));
}

#[test]
fn stdout_is_json_only_no_log_line_leaks() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/a.md", "---\nname: a\n---\nbody\n");

    let out = run(repo.path(), &["search", "widgets"]);
    let text = String::from_utf8(out.stdout).expect("utf8");
    assert_eq!(
        text.trim().lines().count(),
        1,
        "stdout must carry exactly one line: the ResultRecord"
    );
}

#[test]
fn workspace_tools_json_prefix_switches_stderr_to_machine_json() {
    // SC11 end-to-end: the WORKSPACE_TOOLS_ prefix drives the runtime `json`
    // knob, which selects logkit's machine-JSON stderr rendering. The
    // material-effect note (an info log) is emitted to stderr on a
    // core-only run, so stderr carries at least one log line either way.
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/a.md", "---\nname: a\n---\nbody\n");

    let human = run(repo.path(), &["search", "widgets"]);
    let human_stderr = String::from_utf8(human.stderr).expect("utf8");
    let first_human = human_stderr.lines().next().expect("stderr has a log line");
    assert!(
        serde_json::from_str::<Value>(first_human).is_err(),
        "default stderr is logkit's human line, not JSON: {first_human}"
    );

    let json = Command::new(env!("CARGO_BIN_EXE_navigator"))
        .args(["search", "widgets"])
        .current_dir(repo.path())
        .env("HOME", repo.path().join(".home"))
        .env("XDG_CACHE_HOME", repo.path().join(".home/.cache"))
        .env("WORKSPACE_TOOLS_JSON", "true")
        .output()
        .expect("navigator runs");
    let json_stderr = String::from_utf8(json.stderr).expect("utf8");
    let parsed_a_json_line = json_stderr
        .lines()
        .any(|line| serde_json::from_str::<Value>(line).is_ok());
    assert!(
        parsed_a_json_line,
        "WORKSPACE_TOOLS_JSON=true must make stderr logkit's machine JSON: {json_stderr}"
    );
}
