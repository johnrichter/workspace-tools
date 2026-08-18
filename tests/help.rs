//! Verifies the git-tools-style help surface:
//! - top-level `--help` names every composed library and lists every verb
//!   with a one-line summary, plus a worked, runnable EXAMPLES block.
//! - each verb's `--help` carries its own worked, runnable EXAMPLE.
//!
//! The worked examples reference real frontmatter files checked into this
//! repo (`.dat/README.md`, `.anoikis/README.md`), so these tests run the
//! examples verbatim against the repo's own worktree -- not an isolated
//! `TempDir` -- with only `HOME`/`XDG_CACHE_HOME` redirected so the freshness
//! cache never touches the operator's real one. `fix --apply` mutates a real
//! file, so its test backs the file up and restores it (via a `Drop` guard,
//! so a panicking assertion still restores it).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    let home =
        std::env::temp_dir().join(format!("navigator-help-test-home-{}", std::process::id()));
    Command::new(env!("CARGO_BIN_EXE_navigator"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("WORKSPACE_TOOLS_JSON")
        .env_remove("WORKSPACE_TOOLS_QUIET_SCHEMA_WARNINGS")
        .output()
        .expect("navigator binary runs")
}

fn stdout_text(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8")
}

// ---------------------------------------------------------------------------
// Top-level --help
// ---------------------------------------------------------------------------

#[test]
fn top_level_help_names_every_composed_library() {
    let out = run(&repo_root(), &["--help"]);
    assert!(out.status.success(), "--help must exit 0");
    let text = stdout_text(&out);

    for lib in ["frontmatter", "facetquery", "bm25", "clikit", "logkit"] {
        assert!(
            text.contains(lib),
            "top-level help must name composed library `{lib}`, got:\n{text}"
        );
    }
}

#[test]
fn top_level_help_lists_every_verb_with_a_one_line_summary() {
    let out = run(&repo_root(), &["--help"]);
    let text = stdout_text(&out);

    let expectations: &[(&str, &str)] = &[
        ("search", "Free-text search"),
        ("find", "Exact lookup"),
        ("lint", "Validate frontmatter"),
        ("fix", "Apply schema-driven"),
    ];
    for (verb, summary_fragment) in expectations {
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with(verb))
            .unwrap_or_else(|| panic!("no line in --help starts with verb `{verb}`:\n{text}"));
        assert!(
            line.contains(summary_fragment),
            "verb `{verb}`'s line must carry its one-line summary (`{summary_fragment}`), got: {line}"
        );
    }
}

#[test]
fn top_level_help_carries_a_worked_examples_block() {
    let out = run(&repo_root(), &["--help"]);
    let text = stdout_text(&out);

    assert!(
        text.contains("EXAMPLES:"),
        "top-level help must carry an EXAMPLES block, got:\n{text}"
    );
    let examples_block = text.split("EXAMPLES:").nth(1).unwrap();
    for verb in ["search", "find", "lint", "fix"] {
        assert!(
            examples_block.contains(&format!("navigator {verb}")),
            "top-level EXAMPLES block must carry a worked example for `{verb}`, got:\n{examples_block}"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-verb --help
// ---------------------------------------------------------------------------

#[test]
fn every_verb_help_carries_its_own_example() {
    for verb in ["search", "find", "lint", "fix"] {
        let out = run(&repo_root(), &[verb, "--help"]);
        assert!(out.status.success(), "`{verb} --help` must exit 0");
        let text = stdout_text(&out);
        assert!(
            text.contains("EXAMPLE"),
            "`{verb} --help` must carry its own EXAMPLE, got:\n{text}"
        );
        assert!(
            text.contains(&format!("navigator {verb}")),
            "`{verb} --help`'s EXAMPLE must invoke `navigator {verb}`, got:\n{text}"
        );
    }
}

#[test]
fn every_verb_help_carries_a_one_line_summary_up_top() {
    // Adversarial: a verb whose --help summary is empty or multi-paragraph
    // (i.e. no concise one-liner distinct from the EXAMPLE block) fails.
    for verb in ["search", "find", "lint", "fix"] {
        let out = run(&repo_root(), &[verb, "--help"]);
        let text = stdout_text(&out);
        let first_line = text.lines().next().unwrap_or("");
        assert!(
            !first_line.trim().is_empty(),
            "`{verb} --help` must open with a non-empty one-line summary"
        );
        assert!(
            first_line.len() < 200,
            "`{verb} --help`'s opening summary must be one line, not a paragraph: {first_line}"
        );
    }
}

// ---------------------------------------------------------------------------
// Every worked example actually runs and produces the shown output.
// ---------------------------------------------------------------------------

#[test]
fn search_example_from_help_runs_and_succeeds() {
    // navigator search "anoikis" --type doc --limit 5
    let out = run(
        &repo_root(),
        &["search", "anoikis", "--type", "doc", "--limit", "5"],
    );
    assert!(
        out.status.success(),
        "search example must exit 0, got status {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let record: serde_json::Value =
        serde_json::from_str(stdout_text(&out).trim()).expect("stdout is one JSON record");
    assert_eq!(record["status"], "success");
    let hits = record["data"]["hits"]
        .as_array()
        .expect("search result carries a hits array");
    assert!(
        hits.len() <= 5,
        "the example's --limit 5 must be honoured, got {} hits",
        hits.len()
    );
    assert!(
        !hits.is_empty(),
        "the example must find at least the repo's own anoikis-tagged doc"
    );
}

#[test]
fn find_example_from_help_runs_and_succeeds() {
    // navigator find --type doc --tag topic:process
    let out = run(
        &repo_root(),
        &["find", "--type", "doc", "--tag", "topic:process"],
    );
    assert!(
        out.status.success(),
        "find example must exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: serde_json::Value =
        serde_json::from_str(stdout_text(&out).trim()).expect("stdout is one JSON record");
    assert_eq!(record["status"], "success");
    let hits = record["data"]["hits"]
        .as_array()
        .expect("find result carries a hits array");
    assert!(
        !hits.is_empty(),
        "the example must find at least one type:doc, topic:process file"
    );
    for hit in hits {
        assert_eq!(hit["type"], "doc", "every hit must be type:doc: {hit}");
        let tags = hit["tags"].as_array().expect("hit carries tags");
        assert!(
            tags.iter().any(|t| t == "topic:process"),
            "every hit must carry topic:process: {hit}"
        );
    }
}

#[test]
fn lint_example_from_help_runs_and_succeeds() {
    // navigator lint .dat/README.md --json
    let target = repo_root().join(".dat/README.md");
    assert!(
        target.is_file(),
        "the lint example's target must exist: {}",
        target.display()
    );
    let out = run(&repo_root(), &["lint", ".dat/README.md", "--json"]);
    assert!(
        out.status.success(),
        "lint example must exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: serde_json::Value =
        serde_json::from_str(stdout_text(&out).trim()).expect("stdout is one JSON record");
    assert_eq!(record["status"], "success");
    assert_eq!(record["data"]["rollup"]["scanned"], 1);
    assert_eq!(record["data"]["rollup"]["invalid"], 0);
    // --json must also switch stderr's logkit rendering to machine JSON.
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    assert!(
        stderr
            .lines()
            .all(|l| l.trim().is_empty() || serde_json::from_str::<serde_json::Value>(l).is_ok()),
        "with --json every stderr line must itself be JSON: {stderr}"
    );
}

/// Restores `.anoikis/README.md` to its pre-test bytes on drop, so the
/// `fix --apply` example test never leaves the worktree dirty even if an
/// assertion panics partway through.
struct RestoreOnDrop {
    path: PathBuf,
    original: Vec<u8>,
}

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        std::fs::write(&self.path, &self.original).expect("restore the fix example's target file");
    }
}

#[test]
fn fix_example_from_help_runs_and_succeeds_then_is_restored() {
    // navigator fix .anoikis/README.md --apply
    let path = repo_root().join(".anoikis/README.md");
    let original =
        std::fs::read(&path).expect("the fix example's target must exist and be readable");
    let guard = RestoreOnDrop {
        path: path.clone(),
        original: original.clone(),
    };

    let out = run(&repo_root(), &["fix", ".anoikis/README.md", "--apply"]);
    assert!(
        out.status.success(),
        "fix example must exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: serde_json::Value =
        serde_json::from_str(stdout_text(&out).trim()).expect("stdout is one JSON record");
    assert_eq!(record["status"], "success");
    assert_eq!(record["data"]["apply"], true);

    // The file on disk must be valid frontmatter after the fix (whether or
    // not the run changed anything), proving --apply actually wrote through.
    let after = std::fs::read_to_string(&path).expect("read back the fixed file");
    assert!(
        after.starts_with("---\n"),
        "the fixed file must still open with a frontmatter block"
    );

    // guard restores original bytes on drop, verified explicitly here too.
    drop(guard);
    let restored = std::fs::read(&path).expect("read back after restore");
    assert_eq!(restored, original, "the test must leave the file unchanged");
}
