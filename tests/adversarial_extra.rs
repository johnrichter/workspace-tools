//! Test-engineer adversarial supplement: probes acceptance edges tests/cli.rs
//! and tests/adversarial.rs leave untouched -- an actual `fix --apply`
//! mutation, a permission-denied write (exit 70), and an unknown-verb usage
//! failure that clap itself rejects before the app ever builds a
//! `ResultRecord`.

use std::os::unix::fs::PermissionsExt;
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
fn fix_apply_actually_mutates_the_file_on_disk() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/bare.md", "plain body, no frontmatter\n");
    let before = std::fs::read(repo.path().join("docs/bare.md")).unwrap();

    let out = run(repo.path(), &["fix", "--apply", "docs/bare.md"]);
    let record = record(&out);
    assert_eq!(record["status"], "success");
    assert_eq!(record["exit_code"], Value::from(0));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(record["data"]["apply"], Value::from(true));

    let after = std::fs::read(repo.path().join("docs/bare.md")).unwrap();
    assert_ne!(before, after, "an apply run must actually rewrite the file");
    let after_text = String::from_utf8(after).unwrap();
    assert!(
        after_text.starts_with("---\n"),
        "apply must insert a frontmatter block: {after_text}"
    );
}

#[test]
fn permission_denied_write_is_exit_70_not_a_panic_or_internal() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/bare.md", "plain body, no frontmatter\n");
    let target = repo.path().join("docs/bare.md");
    // Deny write on the file itself: fix's write path must surface this as
    // Status::Permission (exit 70), not panic and not misclassify as Internal.
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();

    // Root (and some sandboxed/containerized test runners) can still write
    // through a read-only file's permission bits; the OS-level enforcement
    // this test probes is not universal, so confirm it actually applies
    // here before asserting the CLI's response to it -- otherwise this test
    // would flake to green for the wrong reason under root.
    if std::fs::write(&target, "still writable\n").is_ok() {
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644));
        eprintln!(
            "skipping permission_denied_write_is_exit_70_not_a_panic_or_internal: \
             this runner does not enforce 0o444 write denial (likely root)"
        );
        return;
    }

    let out = run(repo.path(), &["fix", "--apply", "docs/bare.md"]);
    // Restore write perms so TempDir cleanup can remove the file regardless
    // of the assertion outcome below.
    let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644));

    let record = record(&out);
    assert_eq!(
        record["status"], "permission",
        "record: {record}"
    );
    assert_eq!(record["exit_code"], Value::from(70));
    assert_eq!(out.status.code(), Some(70));
    assert!(record["errors"][0]["code"]
        .as_str()
        .unwrap()
        .starts_with("permission."));
}

#[test]
fn unknown_subcommand_is_a_clap_usage_error_exit_2_before_any_result_record() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/a.md", "---\nname: a\n---\nbody\n");

    let out = run(repo.path(), &["frobnicate"]);
    // clap's own arg-parsing failure short-circuits before the app builds a
    // ResultRecord at all: stdout stays empty and clap's own exit code (2)
    // governs, not the clikit taxonomy. Pin this boundary explicitly so a
    // future change that tries to route unknown verbs through clikit is
    // forced to update this test consciously.
    assert!(
        out.stdout.is_empty(),
        "an unrecognized verb must not produce a ResultRecord on stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn no_arguments_at_all_is_also_a_pre_record_clap_usage_error() {
    let repo = TempDir::new().unwrap();
    let out = run(repo.path(), &[]);
    assert!(out.stdout.is_empty());
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn malformed_workspace_tools_json_env_value_is_a_result_record_not_a_panic() {
    // SC5: every verb prints a ResultRecord on stdout and exits via the
    // eleven-member taxonomy -- including when a config source is bad.
    // `WORKSPACE_TOOLS_JSON` is environment-controlled input; a value
    // figment2 can't coerce to bool (e.g. "1", the common shell convention,
    // or "yes") must surface as a config/usage failure inside a
    // ResultRecord, not a raw `.expect()` panic that skips the contract
    // entirely (Rust's own abort exit 101, no JSON at all on stdout).
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/a.md", "---\nname: a\n---\nbody\n");

    let out = Command::new(env!("CARGO_BIN_EXE_navigator"))
        .args(["search", "widgets"])
        .current_dir(repo.path())
        .env("HOME", repo.path().join(".home"))
        .env("XDG_CACHE_HOME", repo.path().join(".home/.cache"))
        .env("WORKSPACE_TOOLS_JSON", "1")
        .output()
        .expect("navigator binary runs");

    assert_ne!(
        out.status.code(),
        Some(101),
        "a malformed env value must not raw-panic (exit 101): stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record = record(&out);
    assert!(
        record.get("status").is_some() && record.get("exit_code").is_some(),
        "stdout JSON must be a ResultRecord with status/exit_code fields: {record}"
    );
}
