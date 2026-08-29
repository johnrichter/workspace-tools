---
name: "frontmatter tag cutover — test verification"
description: "Test-engineer verification of the frontmatter [patch]-to-tag cutover in workspace-tools/Cargo.toml, focused on independently checking the implementer's dependency-resolution blocker claim."
id: "doc:workspace-tools:frontmatter-tag-cutover-test-verification"
tags: [type:doc, topic:process, status:complete, privacy:public, owner:public]
links: []
updated: 2026-08-29T00:00:00Z
---

# frontmatter tag cutover — test verification

Deliverable type: code (Cargo.toml/Cargo.lock dependency-graph change). Verification form: build reproduction across two worktrees (this branch, and unmodified `main` at `bbe368b`), reading exact cargo/git error text, no unit tests written (crate cannot compile in this state for reasons independent of `frontmatter` itself — confirmed below, not assumed).

Worktree tested: `/home/bits/Development/workspaces/psa-platform/workspace-tools/.claude/worktrees/frontmatter-tag-cutover`, commit `b08240a` (branch `chore/frontmatter-tag-cutover`, tip `414d8c8` + report commit `b08240a`), branched from `origin/main` `bbe368b`.
Comparison worktree: temporary `git worktree add .claude/worktrees/main-verify-tmp bbe368b` (unmodified `main`), removed after use — no lasting artifact.

## What I tested

1. `Cargo.toml`/`Cargo.lock` diff review against the brief's scope.
2. CI-realistic path: `cargo build --locked` using the committed `Cargo.lock` exactly as-is, no forced re-resolve — run on both this branch and unmodified `main`.
3. Forced full re-resolve: `cargo update` (no `-p`) — run on both this branch and unmodified `main`, with `CARGO_NET_GIT_FETCH_WITH_CLI=true` to get past a separate sandbox DNS issue and see the real underlying error.
4. Exact error-text inspection to distinguish DNS/SSH-transport failures from "ref not found" failures.
5. Tag/push/version hygiene checks (`git tag -l`, `git ls-remote origin`, `git status`, `git diff` on version line).

Toolchain: `cargo 1.98.0` / `rustc 1.98.0`, matching `mise.toml`'s pin (`rust = "1.98.0"`). The session's provisioned `language-tools` CLI binary was not reachable from this sandboxed shell (`find`/`which` against its install paths were denied by the environment's own tool-use gate); ran `cargo` directly instead, which is what that CLI wraps for a Rust `build`/`test` gate — the dependency-resolution failure below happens before any language-tools-specific behavior would matter, so this substitution does not weaken the check.

## The critical claim — findings

**All three blocker scenarios apply, each to a different, correctly-separable part of the picture. This is not a single either/or; the evidence supports a combination:**

### Scenario 1 (real, general Cargo requirement) — CONFIRMED

`cargo update` (full re-resolve, no `Cargo.toml` edits) on **unmodified `main`** (`bbe368b`) fails identically to the branch's failure:

```
$ CARGO_NET_GIT_FETCH_WITH_CLI=true cargo update
    Updating git repository `https://github.com/johnrichter/claude-shared-tooling.git`
fatal: couldn't find remote ref refs/tags/rust/bm25/v0.1.0
[... 3 retries, same message ...]
error: failed to get `bm25` as a dependency of package `navigator v2.0.0 (.../main-verify-tmp)`
Caused by:
  failed to load source for dependency `bm25`
Caused by:
  unable to update https://github.com/johnrichter/claude-shared-tooling.git?tag=rust%2Fbm25%2Fv0.1.0
Caused by:
  failed to fetch into: /home/bits/.cargo/git/db/claude-shared-tooling-75d6b1546e1c6119
Caused by:
  process didn't exit successfully: `git fetch ... refs/tags/rust/bm25/v0.1.0 ...` (exit status: 128)
```

This confirms the implementer's core claim: any full dependency-graph re-resolution needs every patched git source's own tag ref to be fetchable, regardless of the local-path `[patch]` override, and this is pre-existing on `main` — not introduced by this change. `bm25` (not `frontmatter`) is the crate that fails first alphabetically; same story would recur for `facetquery`/`clikit`/`logkit` once `bm25` were somehow fixed.

### Scenario 2 (narrower gap specific to this task's own approach) — ALSO CONFIRMED, and more consequential than the report frames it

The report frames the CI-realistic path (`cargo build`/`test` using the existing lock, no forced re-resolve) as safe in general and only broken by this task because the *whole crate* can't build due to the pre-existing bm25 gap. I tested this precisely and found a sharper result:

```
main (bbe368b), cargo build --locked:
   Compiling navigator v2.0.0 (.../main-verify-tmp)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.59s
```
Succeeds, zero network calls — main's `Cargo.lock` has no `source =` line for any of the five crates (fully satisfied by path-patch shape), so `--locked` never touches the network.

```
this branch (414d8c8), cargo build --locked:
    Updating git repository `https://github.com/johnrichter/claude-shared-tooling.git`
warning: spurious network error (3 tries remaining): failed to resolve address for github-johnrichter...
error: failed to get `bm25` as a dependency of package `navigator v2.1.0 (.../frontmatter-tag-cutover)`
Caused by:
  failed to load source for dependency `bm25`
...
```

Fails — and it fails trying to fetch **`bm25`**, not `frontmatter`. This is the sharper finding: adding the single `source = "git+...frontmatter..."` line to `Cargo.lock`'s `frontmatter` entry (this task's own hand-edit, standing in for `cargo update -p frontmatter`) is enough to force Cargo into re-verifying/re-resolving the *whole* graph even under `--locked`, which then hits the same pre-existing `bm25` ref gap that `main` never hits under ordinary `--locked` builds. So: this task's approach converts a **latent**, resolve-only risk (main is exposed to it only if someone runs `cargo update` or deletes the lock) into an **active** one — plain `cargo build`/`cargo test`/CI on this branch, as committed, cannot succeed at all today, for anyone, until the other four crates are tagged or the strategy changes. That is a real, immediate regression in "does this branch build," not just a documented future risk. The report's blocker section is honest about the end state (full crate doesn't build) but underclaims how it got there — it did not isolate that `--locked` itself, with no update command at all, is what breaks, purely because of the lockfile hand-edit.

### Scenario 3 (sandbox-specific artifact) — CONFIRMED as a real, separate, correctly-diagnosed issue, NOT a disguise for scenario 1/2

Without `CARGO_NET_GIT_FETCH_WITH_CLI=true`, every fetch attempt (on both branch and main) fails with:
```
failed to resolve address for github-johnrichter: Name or service not known; class=Net (12)
```
This is caused by this sandbox's own global `~/.gitconfig` `url.*.insteadOf` rewrites (`https://github.com/johnrichter/` → `git@github-johnrichter:johnrichter/`, confirmed via `git config --global --get-regexp insteadof`), which libgit2's built-in transport cannot resolve as a real hostname. Setting the CLI-fetch flag switches to the system `git` binary, which has an SSH `Host github-johnrichter` alias configured and gets past this — after which the error changes character entirely, to `fatal: couldn't find remote ref refs/tags/rust/bm25/v0.1.0`, a plain "the ref doesn't exist upstream" error with no DNS/transport content at all. These are genuinely two different failures, and the implementer's report correctly keeps them separate rather than conflating them (their write-up already draws this same DNS-vs-ref-not-found line, and my independent reproduction supports it verbatim).

## Cargo.toml diff review

- `[patch."...claude-shared-tooling.git"]` table: only the `frontmatter = { path = ... }` line removed. Confirmed via `git diff bbe368b..HEAD -- Cargo.toml`, filtered to added/removed lines — `bm25`/`facetquery`/`clikit`/`logkit` entries produce zero diff lines (byte-for-byte untouched).
- `frontmatter`'s `[dependencies]` entry (`{ git = "https://github.com/johnrichter/claude-shared-tooling.git", tag = "rust/frontmatter/v0.1.0" }`) was already present pre-change and is identical in shape to the other four entries (same key names, same URL, only the tag path differs) — matches this file's own established tagged-git-dep convention. No dependency-line edit was needed or made.
- Version bump: `2.0.0` → `2.1.0` is the only version-related change in the diff; no `[patch]`, `[dependencies]`, or other `[package]` key touched besides `version`.
- Two comment-block edits (above `[dependencies]`, above `[patch]`): read both in full. Each is wording-only, keeps the original structure and the other four crates' claims (still lacking tags, still patched) unchanged, and correctly updates only the `frontmatter`-specific claims to reflect this change. Proportionate — not scope creep in effect, even though it technically touches lines outside the single `[patch]` line the brief named. Flagging to quality-reviewer per the implementer's own hand-off note, but my own read is this is fine to accept as-is; reverting is a trivial two-hunk change if the reviewer disagrees.
- `Cargo.lock`: single added line, `source = "git+https://github.com/johnrichter/claude-shared-tooling.git?tag=rust/frontmatter/v0.1.0#0a265b194aba30fe6fc1aca62832c78088b783a8"`, on the existing `frontmatter` package entry — matches the tag/commit the report's own `ls-remote` output verified (`061a3296...` tag object dereferencing to `0a265b19...`). This is the single line whose *presence* is what triggers the Scenario 2 re-resolution finding above; it is otherwise exactly the shape a real `cargo update -p frontmatter` would write.

## Hygiene checks

- `git tag -l`: only pre-existing `v2.0.0` and an unrelated `backup/sample-value-cleanup/...` tag. No new tag created by this change.
- `git ls-remote origin` / `git branch -r`: `chore/frontmatter-tag-cutover` does not exist on `origin`. `git status`: working tree clean, branch ahead of `origin/main` by 2 local commits, nothing pushed.

## Acceptance

| Criterion | Result | Evidence |
|---|---|---|
| `[patch]` table: only `frontmatter` removed, other four byte-for-byte untouched | PASS | `git diff` filtered to +/- lines shows zero lines for bm25/facetquery/clikit/logkit |
| `frontmatter` dependency declaration matches existing tagged-git-dep convention | PASS | identical shape to other four `[dependencies]` entries, unchanged by this diff |
| Version bumped 2.0.0 → 2.1.0, no tag created | PASS | diff + `git tag -l` |
| Branch/worktree only, nothing pushed | PASS | `git status`, `git ls-remote origin` |
| "Confirm the crate now resolves and builds from the real tag" (full crate) | FAIL, but for a pre-existing, out-of-scope reason confirmed independent of this change | see Scenario 1/2 above — full crate cannot build until `bm25`/`facetquery`/`clikit`/`logkit` are tagged, or the patch/pin strategy changes; this is a real, general Cargo limitation reproduced on unmodified `main` too, not something introduced by or fixable within this task |
| Comment-block edits proportionate | PASS (with a flag for quality-reviewer's own judgment call, not a defect) | see review above |

## Failures

1. **`cargo build --locked` on this branch fails** (exit 101), fetching `refs/tags/rust/bm25/v0.1.0` → `fatal: couldn't find remote ref`. Repro: `cd .../frontmatter-tag-cutover && CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --locked`. Root cause: pre-existing missing tags on `bm25`/`facetquery`/`clikit`/`logkit` (confirmed via `git ls-remote`, no such refs exist upstream), triggered into visibility by this task's own `Cargo.lock` hand-edit for `frontmatter` forcing a graph re-verify. Not fixable inside this task's scope (would require tagging the other four crates or changing the patch/pin strategy, per the report's own reconciliation note, which I independently agree with).
2. Not a flake — reproduced identically across two separate runs on the branch and confirmed structurally identical (same missing ref, different first-failing crate is impossible since alphabetical order is deterministic) on unmodified `main` via `cargo update`.

No test suite was written or run beyond the above build reproductions: `cargo test` would fail at the identical pre-compile dependency-resolution step, before any Rust source is reached, so it adds no new evidence.

## CI/e2e

Not run — no CI config invoked beyond the direct `cargo` commands above, which mirror what `mise.toml`'s pinned toolchain would run. `language-tools` CLI itself was not reachable in this sandboxed session (see toolchain note above); this does not change the outcome since the failure occurs at Cargo's own dependency-resolution stage, upstream of anything `language-tools` adds.

## Verdict

**PASS WITH CONCERNS.**

The change itself (the `Cargo.toml`/`Cargo.lock` diff, scope, comments, hygiene) is correct and matches the brief exactly. The implementer's blocker claim is substantively true and independently reproduced: this is a real, pre-existing, general Cargo limitation (Scenario 1, confirmed via `cargo update` on unmodified `main`), not a sandbox artifact (Scenario 3 is a separate, correctly-diagnosed DNS/SSH issue that does not explain the ref-not-found failures). The one correction to the implementer's own framing: the everyday, CI-realistic path (`cargo build`/`test` with the existing lock, no forced re-resolve) is not merely "still exposed to the same latent risk as before" — it is actively broken by this branch specifically (Scenario 2), because the single `Cargo.lock` hand-edit this task made is, by itself, enough to force full re-resolution under plain `cargo build --locked`, something `main` never triggers. Net effect: as committed, this branch cannot build or test the full `navigator` crate at all, today, for anyone, and that is a direct, immediate consequence of this task's own lockfile edit landing before the other four crates have tags — worth the quality-reviewer's and orchestrator's explicit attention before this lands or before `bm25`/`facetquery`/`clikit`/`logkit` cutovers are scheduled, even though nothing about the diff itself needs to change to fix it (the fix has to happen upstream, in tagging or the patch/pin strategy).
