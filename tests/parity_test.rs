//! SC4 subcommand-parity gate. Provisions the published `v2.0.0` release
//! binary the way an operator does -- download the archive for this runner's
//! OS/arch, verify its sha256 against the release `checksums.txt`, extract it
//! -- and asserts the binary answers `search`, `find`, `lint` and `fix` in
//! agreement with the behaviour recorded in `sc0-parity-baseline.md` (the
//! reference every parity assertion in the navigator-workspace-tools-port
//! project cites). This gate unlocks the project's irreversible deletions, so
//! it fails closed: any provisioning failure, checksum mismatch, or
//! behavioural divergence panics, which exits `cargo test` non-zero.
//!
//! Parity is behavioural, mapped through the one contract change the project
//! sanctions. The SC0 baseline was captured from the `0.1.0` binary, which
//! printed terse human text and a bare per-verb JSON payload. SC5 re-envelopes
//! every verb's output as a `clikit` `ResultRecord` on stdout and moves the
//! per-verb payload verbatim under that record's `data` field, and re-maps each
//! verb's ad-hoc exit code onto the fleet exit taxonomy. So parity here asserts
//! that (a) each SC0-recorded payload shape survives intact as `data`, and
//! (b) each SC0-recorded verdict survives -- success stays exit 0, the lint
//! gate's "no" answer stays a non-zero non-success verdict (SC0's exit 1 is now
//! the taxonomy's `gate_negative`/20), and clap-level argument rejections keep
//! their identical pre-taxonomy exit 2. A regression in any of these fails the
//! gate; the SC5 re-enveloping itself is expected, not a divergence.
//!
//! Provisioning: set `WORKSPACE_TOOLS_PARITY_BINARY` to an already-provisioned
//! `v2.0.0` binary to run fully offline (CI can provision once and pass the
//! path); otherwise the binary is downloaded and verified on first use and
//! cached under the system temp dir for the rest of the run.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The release under test. The plugin repins to this exact tag, and the
/// binary tested here is byte-identical to the one the plugin provisions, so
/// this gate stands in for both.
const VERSION: &str = "2.0.0";
const RELEASE_BASE: &str =
    "https://github.com/johnrichter/workspace-tools/releases/download/v2.0.0";
/// Env override: a path to an already-provisioned `v2.0.0` binary. When set the
/// download/checksum steps are skipped (the operator vouches for the bytes),
/// but the version is still asserted so the gate never runs against the wrong
/// build.
const BINARY_OVERRIDE_ENV: &str = "WORKSPACE_TOOLS_PARITY_BINARY";

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// The provisioned release binary, resolved once per test process. Panics
/// (fails the gate) if the binary cannot be provisioned or is not `v2.0.0`.
fn provisioned_binary() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let bin = match std::env::var_os(BINARY_OVERRIDE_ENV) {
            Some(path) => PathBuf::from(path),
            None => download_and_verify(),
        };
        assert_version(&bin);
        bin
    })
    .as_path()
}

/// `navigator_<version>_<os>_<arch>.tar.gz` for this runner. The release ships
/// linux/darwin x amd64/arm64; any other target cannot be tested and fails
/// closed rather than skipping the gate.
fn archive_name() -> String {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => panic!("no release archive for OS {other:?}; cannot run the parity gate"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("no release archive for arch {other:?}; cannot run the parity gate"),
    };
    format!("navigator_{VERSION}_{os}_{arch}.tar.gz")
}

/// Downloads the runner's archive and the release `checksums.txt`, verifies the
/// archive digest against the checksum record, extracts it, and returns the
/// path to the extracted binary. The extraction dir is cached under the system
/// temp dir and reused if it already holds a matching `v2.0.0` binary, so a
/// process downloads at most once. Any step failing panics -- the gate refuses
/// to run against an unverified binary.
fn download_and_verify() -> PathBuf {
    let cache = std::env::temp_dir().join(format!("navigator-parity-v{VERSION}"));
    let extracted = cache.join("navigator");
    if extracted.is_file() && binary_version(&extracted).as_deref() == Some(VERSION) {
        return extracted;
    }

    // Provision into a private temp dir, verify, then publish to the cache dir
    // atomically enough for a single process: partial state never satisfies the
    // reuse check above because the version probe would fail.
    let staging = TempDir::new().expect("create parity staging dir");
    let archive = staging.path().join(archive_name());
    let checksums = staging.path().join("checksums.txt");
    fetch(&format!("{RELEASE_BASE}/{}", archive_name()), &archive);
    fetch(&format!("{RELEASE_BASE}/checksums.txt"), &checksums);

    let expected = expected_digest(&checksums, &archive_name());
    let actual = sha256_hex(&archive);
    assert_eq!(
        actual, expected,
        "sha256 of {} does not match checksums.txt -- refusing to run parity against unverified bytes",
        archive_name()
    );

    let status = Command::new("tar")
        .args(["-xzf", archive.to_str().unwrap(), "-C"])
        .arg(staging.path())
        .status()
        .expect("run tar to extract the release archive");
    assert!(status.success(), "tar failed to extract {}", archive_name());

    let staged_bin = staging.path().join("navigator");
    assert!(
        staged_bin.is_file(),
        "release archive did not contain a `navigator` binary"
    );
    std::fs::create_dir_all(&cache).expect("create parity cache dir");
    std::fs::copy(&staged_bin, &extracted).expect("publish provisioned binary to cache");
    set_executable(&extracted);
    extracted
}

/// Fetches `url` to `dest` with curl, failing closed on any transport error.
fn fetch(url: &str, dest: &Path) {
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "--connect-timeout", "30", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .expect("run curl -- required to provision the release binary");
    assert!(status.success(), "curl failed to download {url}");
}

/// The expected hex digest for `archive` from a `sha256sum`-format
/// `checksums.txt` (`<hex>  <filename>` per line).
fn expected_digest(checksums: &Path, archive: &str) -> String {
    let text = std::fs::read_to_string(checksums).expect("read checksums.txt");
    text.lines()
        .find_map(|line| {
            let (hex, name) = line.split_once("  ")?;
            (name.trim() == archive).then(|| hex.trim().to_string())
        })
        .unwrap_or_else(|| panic!("checksums.txt has no entry for {archive}"))
}

fn sha256_hex(path: &Path) -> String {
    use std::fmt::Write as _;
    let bytes = std::fs::read(path).expect("read archive for hashing");
    let digest = Sha256::digest(&bytes);
    digest.iter().fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).expect("stat binary").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("mark binary executable");
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

/// `--version` reports `navigator <semver>`; returns the semver, or `None` if
/// the binary cannot be run.
fn binary_version(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    text.trim().strip_prefix("navigator ").map(str::to_string)
}

fn assert_version(bin: &Path) {
    let reported = binary_version(bin).unwrap_or_else(|| {
        panic!(
            "provisioned binary at {} did not run --version",
            bin.display()
        )
    });
    assert_eq!(
        reported, VERSION,
        "provisioned binary reports {reported}, not the {VERSION} under parity"
    );
}

// ---------------------------------------------------------------------------
// Invocation helpers
// ---------------------------------------------------------------------------

/// Runs a verb in a hermetic repo: cwd is the temp repo and `HOME`/
/// `XDG_CACHE_HOME` are redirected inside it, so the reachable-set walk sees
/// only the test files and the freshness cache never touches the operator's.
/// The `WORKSPACE_TOOLS_` knobs are cleared so the run shows the default
/// output shape SC0 recorded.
fn run(repo: &Path, args: &[&str]) -> Output {
    Command::new(provisioned_binary())
        .args(args)
        .current_dir(repo)
        .env("HOME", repo.join(".home"))
        .env("XDG_CACHE_HOME", repo.join(".home/.cache"))
        .env_remove("WORKSPACE_TOOLS_JSON")
        .env_remove("WORKSPACE_TOOLS_QUIET_SCHEMA_WARNINGS")
        .output()
        .expect("run the provisioned navigator binary")
}

fn write(repo: &Path, rel: &str, contents: &str) {
    let path = repo.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Parses stdout as the single `ResultRecord` and returns its `data` payload,
/// after checking the envelope invariants that carry the SC0 payload: exactly
/// one JSON object on stdout (a stdout consumer never sees the stderr advisory
/// note), the right `command`, and a non-null `data`.
fn record_data(out: &Output, verb: &str) -> Value {
    let stdout = String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8");
    assert_eq!(
        stdout.trim().lines().count(),
        1,
        "stdout must be exactly one line (the ResultRecord); got: {stdout}"
    );
    let record: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON object ({e}): {stdout}"));
    assert_eq!(
        record["command"],
        serde_json::json!(["navigator", verb]),
        "ResultRecord command names the verb"
    );
    let data = record["data"].clone();
    assert!(data.is_object(), "{verb} carries a data payload: {record}");
    data
}

/// The advisory note SC0 records on stderr for every verb -- present in the
/// default (non-quiet) run, and on stderr so a stdout JSON consumer never sees
/// it.
fn assert_advisory_on_stderr(out: &Output) {
    let stderr = String::from_utf8(out.stderr.clone()).expect("stderr is UTF-8");
    assert!(
        stderr.contains("results reflect the merged profile resolved for this repo"),
        "the merged-profile advisory note must go to stderr: {stderr}"
    );
}

/// Asserts an object has each field, and that each is non-null unless named in
/// `nullable` (SC0 records several `T|null` fields).
fn assert_shape(obj: &Value, ctx: &str, required: &[&str], nullable: &[&str]) {
    let map = obj
        .as_object()
        .unwrap_or_else(|| panic!("{ctx} is not a JSON object: {obj}"));
    for key in required {
        assert!(
            map.contains_key(*key),
            "{ctx} is missing field {key:?}: {obj}"
        );
        if !nullable.contains(key) {
            assert!(!map[*key].is_null(), "{ctx} field {key:?} is null: {obj}");
        }
    }
}

fn assert_number(v: &Value, ctx: &str) {
    assert!(v.is_number(), "{ctx} must be a number: {v}");
}

// ---------------------------------------------------------------------------
// SC0 parity assertions, per verb
// ---------------------------------------------------------------------------

#[test]
fn provisioned_binary_is_the_v2_0_0_release() {
    // Forces provisioning (download + checksum verify + extract) and asserts
    // the version. Every other test depends on this succeeding.
    assert_version(provisioned_binary());
}

#[test]
fn verb_set_matches_sc0_baseline() {
    // SC0 records exactly five commands: search, find, lint, fix, help.
    let out = Command::new(provisioned_binary())
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(out.status.success(), "--help exits 0");
    let help = String::from_utf8(out.stdout).expect("help is UTF-8");
    for verb in ["search", "find", "lint", "fix", "help"] {
        assert!(
            help.contains(verb),
            "--help must list the {verb:?} command (SC0 verb list): {help}"
        );
    }
}

#[test]
fn search_matches_sc0_baseline() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "docs/a.md",
        "---\nname: widget-doc\ntags:\n  - type:project\n---\nnavigator widgets and gears\n",
    );

    let out = run(repo.path(), &["search", "widgets", "--limit", "3"]);
    assert_eq!(out.status.code(), Some(0), "search on a hit exits 0 (SC0)");
    assert_advisory_on_stderr(&out);

    // SC0 search payload: the result envelope plus a ranked hit list.
    let data = record_data(&out, "search");
    assert_shape(
        &data,
        "search data",
        &["degraded", "degraded_reason", "fix_command", "hits"],
        &["degraded_reason", "fix_command"],
    );
    let hits = data["hits"].as_array().expect("search hits is an array");
    assert!(!hits.is_empty(), "search over a matching doc returns a hit");
    // SC0 search-hit shape.
    assert_shape(
        &hits[0],
        "search hit",
        &[
            "path",
            "score",
            "hint",
            "matched",
            "tags",
            "id",
            "conformant",
            "violations",
        ],
        &["hint", "id"],
    );
    assert_number(&hits[0]["score"], "search hit score");
    assert!(hits[0]["matched"].is_array(), "matched is an array");
    assert!(hits[0]["tags"].is_array(), "tags is an array");
    assert!(hits[0]["violations"].is_array(), "violations is an array");
}

#[test]
fn find_matches_sc0_baseline() {
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        "docs/a.md",
        "---\nname: a\ntags:\n  - type:project\n---\nbody\n",
    );

    // SC0: a no-hit result set is not an error -- empty hits, exit 0.
    let out = run(repo.path(), &["find", "--type", "nonexistent-type-xyz"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "find with no hits exits 0 (SC0)"
    );
    assert_advisory_on_stderr(&out);
    let data = record_data(&out, "find");
    assert_shape(
        &data,
        "find data",
        &["degraded", "degraded_reason", "fix_command", "hits"],
        &["degraded_reason", "fix_command"],
    );
    assert_eq!(
        data["hits"],
        serde_json::json!([]),
        "a no-hit find returns an empty hit list, not an error (SC0)"
    );

    // SC0: `--limit` is a `search`-only flag; `find --limit` is a clap
    // argument-parse rejection at exit 2 -- identical pre-taxonomy behaviour,
    // so this exit code is asserted exactly.
    let rejected = run(repo.path(), &["find", "--type", "project", "--limit", "2"]);
    assert_eq!(
        rejected.status.code(),
        Some(2),
        "find --limit is a clap usage error at exit 2 (SC0)"
    );
    let stderr = String::from_utf8(rejected.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("--limit"),
        "the clap error names the rejected --limit flag: {stderr}"
    );
}

#[test]
fn lint_matches_sc0_baseline() {
    let repo = TempDir::new().unwrap();
    // An all-valid scope: one file with parseable frontmatter.
    write(repo.path(), "docs/ok.md", "---\nname: ok\n---\nbody\n");

    let valid = run(repo.path(), &["lint", "docs/ok.md"]);
    assert_eq!(
        valid.status.code(),
        Some(0),
        "an all-valid lint scope exits 0 (SC0)"
    );
    assert_advisory_on_stderr(&valid);
    let data = record_data(&valid, "lint");
    assert_shape(&data, "lint data", &["files", "rollup"], &[]);
    assert!(data["files"].is_array(), "lint files is an array");
    let rollup = &data["rollup"];
    for key in ["scanned", "valid", "invalid", "missing_frontmatter"] {
        assert_number(&rollup[key], &format!("lint rollup.{key}"));
    }
    assert_eq!(rollup["missing_frontmatter"], serde_json::json!(0));
    // SC0 lint-file shape.
    let file = data["files"]
        .as_array()
        .and_then(|f| f.first())
        .expect("lint scanned a file");
    assert_shape(
        file,
        "lint file",
        &[
            "path",
            "file_class",
            "is_valid",
            "missing_frontmatter",
            "violations",
        ],
        &["file_class"],
    );

    // SC0: a scope with a missing-frontmatter file is the gate's "no" answer --
    // a non-zero, non-success verdict. SC0 recorded exit 1; SC5 re-maps that
    // same verdict onto the taxonomy's gate_negative/20.
    write(repo.path(), "docs/bare.md", "plain body, no frontmatter\n");
    let findings = run(repo.path(), &["lint", "docs"]);
    assert_ne!(
        findings.status.code(),
        Some(0),
        "a lint scope with a finding is a non-zero gate verdict (SC0's exit 1)"
    );
    let fdata = record_data(&findings, "lint");
    assert!(
        fdata["rollup"]["missing_frontmatter"]
            .as_i64()
            .expect("missing_frontmatter is a number")
            >= 1,
        "the missing-frontmatter file is counted in the rollup"
    );
}

#[test]
fn fix_matches_sc0_baseline() {
    let repo = TempDir::new().unwrap();
    write(repo.path(), "docs/bare.md", "plain body, no frontmatter\n");
    let target = repo.path().join("docs/bare.md");
    let before = std::fs::read(&target).unwrap();

    // SC0: dry-run (no --apply) reports the planned change but leaves the file
    // byte-identical, at exit 0.
    let dry = run(repo.path(), &["fix", "docs/bare.md"]);
    assert_eq!(dry.status.code(), Some(0), "fix dry-run exits 0 (SC0)");
    assert_advisory_on_stderr(&dry);
    let data = record_data(&dry, "fix");
    assert_eq!(
        data["apply"],
        serde_json::json!(false),
        "a dry-run reports apply=false (SC0)"
    );
    assert_shape(&data, "fix data", &["apply", "files", "rollup"], &[]);
    let rollup = &data["rollup"];
    for key in ["scanned", "changed", "unfixable", "human_authored_pending"] {
        assert_number(&rollup[key], &format!("fix rollup.{key}"));
    }
    // SC0 fix-file shape.
    let file = data["files"]
        .as_array()
        .and_then(|f| f.first())
        .expect("fix reported a file");
    assert_shape(
        file,
        "fix file",
        &[
            "path",
            "file_class",
            "action",
            "violations_before",
            "changed",
            "human_authored_fields",
            "workspace_nested",
            "unfixable_reason",
        ],
        &["file_class", "unfixable_reason"],
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "a dry run must not touch the file (SC0)"
    );

    // SC0: --apply reports apply=true and rewrites the file on disk.
    let applied = run(repo.path(), &["fix", "docs/bare.md", "--apply"]);
    assert_eq!(applied.status.code(), Some(0), "fix --apply exits 0 (SC0)");
    let applied_data = record_data(&applied, "fix");
    assert_eq!(
        applied_data["apply"],
        serde_json::json!(true),
        "an applied fix reports apply=true (SC0)"
    );
    assert_ne!(
        std::fs::read(&target).unwrap(),
        before,
        "--apply must rewrite the file on disk (SC0)"
    );
}
