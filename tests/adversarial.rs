//! Supplementary adversarial coverage beyond `tests/cli.rs`: an unsupported
//! sentinel version, a boolean facetquery grouping, and a zero-file scan --
//! each asserted against the clikit `ResultRecord` contract.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn run(repo: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_navigator"))
        .args(args)
        .current_dir(repo)
        .env("HOME", repo.join(".home"))
        .env("XDG_CACHE_HOME", repo.join(".home/.cache"))
        .env_remove("WORKSPACE_TOOLS_JSON")
        .output()
        .expect("navigator binary runs")
}

fn write(repo: &Path, rel: &str, contents: &str) {
    let path = repo.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn record(out: &Output) -> Value {
    let text = String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8");
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("stdout not JSON ({e}): {text}"))
}

#[test]
fn unsupported_sentinel_version_is_precondition_unmet_exit_30() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "navigator.toml",
        "sentinel_version = 999\nextensions = []\n\n[schema]\nprofile = \"core@2.0.0\"\n",
    );

    let out = run(repo.path(), &["lint"]);
    let record = record(&out);
    assert_eq!(record["status"], "precondition_unmet");
    assert_eq!(record["exit_code"], Value::from(30));
    assert!(record["errors"][0]["code"]
        .as_str()
        .unwrap()
        .starts_with("precondition_unmet."));
    assert_eq!(out.status.code(), Some(30));
}

#[test]
fn boolean_grouped_query_runs_and_is_success() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "docs/a.md",
        "---\nname: a\ntags:\n  - type:skill\n  - topic:apm\n---\nbody\n",
    );

    let out = run(
        repo.path(),
        &["find", "(type:skill OR type:agent) AND NOT topic:billing"],
    );
    let record = record(&out);
    assert_eq!(record["status"], "success");
    assert!(record["data"]["hits"].is_array());
}

#[test]
fn zero_file_scan_is_success_with_empty_hits() {
    let repo = TempDir::new().unwrap();
    // A reachable set with no `.md` files at all.
    write(repo.path(), "README.txt", "not markdown\n");

    let out = run(repo.path(), &["search", "anything"]);
    let record = record(&out);
    assert_eq!(record["status"], "success");
    assert_eq!(record["data"]["hits"], serde_json::json!([]));
}
