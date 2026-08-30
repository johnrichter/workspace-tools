---
name: "frontmatter v0.2.0 bump — test verification"
description: "Independent test-engineer verification of the frontmatter v0.1.0 -> v0.2.0 dependency bump (Cargo.toml/Cargo.lock hand-edit)."
id: "doc:workspace-tools:frontmatter-v0.2.0-bump-test-verification"
tags: [type:doc, topic:process, status:complete, privacy:public, owner:public]
links: []
updated: 2026-08-30T00:00:00Z
---

# frontmatter v0.1.0 -> v0.2.0 bump — independent verification

## Deliverable type

Code (dependency-manifest change) + hand-edited lockfile. No new test surface expected
(pure dependency bump); verification is by diff inspection, remote-tag confirmation,
fresh build/test/lint, and consumer-usage grep — the forms the dispatch specified.

## 1. Diff scope: `git diff main -- Cargo.toml Cargo.lock`

```
Cargo.toml: one line changed -- frontmatter git tag rust/frontmatter/v0.1.0 -> v0.2.0.
Cargo.lock: one line changed -- frontmatter `source` line's tag and commit hash to match.
```
`version = "0.1.0"` and the `dependencies` array of the `frontmatter` lock entry are
unchanged, as reported. Full-tree diff (`git diff main --stat`) confirms only
`Cargo.toml`, `Cargo.lock`, and the implementer's own report file changed — no other
file touched.

**PASS** — diff is exactly the claimed tag/commit swap, nothing else.

## 2. Commit hash authenticity

`git ls-remote --tags https://github.com/johnrichter/claude-shared-tooling.git rust/frontmatter/v0.2.0`
returned:
```
4c7e098b2b315905a6753086b943fcff53d5c607	refs/tags/rust/frontmatter/v0.2.0
```
Matches `Cargo.lock`'s `source` line commit exactly.

**PASS** — hash is the real, remote-resolved commit for the v0.2.0 tag, not invented.

## 3. Build / test / lint, run fresh

- `cargo build --locked`: passed initial run (cache-warm); re-verified with
  `cargo clean -p frontmatter` followed by `cargo build --locked -v` — forced a fresh
  `Compiling frontmatter v0.1.0 (...tag=rust%2Ffrontmatter%2Fv0.2.0#4c7e098b)` from
  `~/.cargo/git/checkouts/claude-shared-tooling-*/4c7e098/rust/frontmatter/`, confirming
  the build actually resolves and compiles the new commit, not a stale cached artifact.
  Result: clean build, no errors.
- `cargo test --locked` (post fresh-frontmatter-build): all suites green —
  unit (244), `adversarial` (3), `adversarial_extra` (5), `cli` (8), `help` (9),
  `parity_test` (6). 275 total, 0 failed, 0 ignored.
- `cargo clippy --locked --all-targets`: re-run after a second `cargo clean -p
  frontmatter` to force a fresh `Checking frontmatter ...` line — clean, no warnings.

**PASS** — build/test/clippy all green against a freshly-compiled frontmatter v0.2.0,
not a warm-cache artifact carried over from before the bump.

## 4. Consumer does not touch the new `at_most_one` cardinality feature

Confirmed independently, two ways:

- Fetched-checkout inspection (`~/.cargo/git/checkouts/claude-shared-tooling-*/4c7e098/
  rust/frontmatter/src/fix.rs`): `at_most_one` cardinality (`Cardinality::AtMostOne`) is
  real and present in the fetched v0.2.0 source — corroborates the report's description
  of the added feature, rather than taking the report's word for what changed upstream.
- `grep -rn "frontmatter::" src/` in this repo: consumer calls `parse`, `validate`
  (via `validate::fold`), `matches`, `Profile`, `ParsedFrontmatter`, `Violation`,
  `CoverageRollup`, `RawFields`, `ScanOutcome`, `MergeWarning`, `ProfileError`,
  `Dimension`, `embedded_pack_json`/`embedded_core_json`, `propose_fix`,
  `propose_skeleton`, `render`. No reference to `Cardinality`, `AtMostOne`, or any
  profile JSON with `"cardinality": "at_most_one"` in this repo's own fixtures/source.

**PASS** — this consumer's usage surface does not exercise the new feature; the bump is
additive-only from this repo's perspective.

## Acceptance

| Criterion | Result |
|---|---|
| `Cargo.toml`/`Cargo.lock` diff is scoped to the tag/commit bump only | PASS |
| Lockfile commit hash is real and matches the remote tag | PASS |
| Build/test/clippy green on a freshly-compiled frontmatter v0.2.0 | PASS |
| Consumer usage does not touch the new `at_most_one` cardinality feature | PASS |

## Failures

None found.

## CI / e2e

Not applicable — no CI pipeline invoked in this task; local `cargo build`/`test`/
`clippy --locked` is the full verification surface per the dispatch and the task's own
`test_strategy` (pure dependency bump, existing suite is the regression net).

## Verdict

**PASS** — all four independent checks confirm the implementer's report: this is a
scoped, hash-verified, build/test/lint-clean, feature-inert dependency bump.
