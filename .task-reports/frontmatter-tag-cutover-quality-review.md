---
name: "frontmatter tag cutover — quality review"
description: "Quality review of the frontmatter [patch]-to-tag cutover in workspace-tools: root-causes the cargo build --locked regression to two Cargo.lock inconsistencies, fixes both, and re-verifies build/test/clippy including a cold-git-cache run."
id: "doc:workspace-tools:frontmatter-tag-cutover-quality-review"
tags: [type:doc, topic:process, status:complete, privacy:public, owner:public]
links: [doc:workspace-tools:frontmatter-tag-cutover-report, doc:workspace-tools:frontmatter-tag-cutover-test-verification]
updated: 2026-08-29T00:00:00Z
---

# frontmatter tag cutover — quality review

## Verdict

**ACCEPT WITH FIXES.** The blocker was real and correctly reported, but it was *not* out of scope. Its root cause was two incomplete-lockfile defects inside this task's own change, both fixable here. Both are fixed. `cargo build --locked` now succeeds on this branch, including from a completely cold git cache, and compiles `frontmatter` from the real `rust/frontmatter/v0.1.0` tag. This branch is safe to merge and does **not** need to wait on tags for `bm25`/`facetquery`/`clikit`/`logkit`.

## Headline correction to the two prior reports

Both prior reports concluded the full-crate build failure was caused by the pre-existing missing tags on the other four crates and was therefore unfixable inside this task. That conclusion is **wrong**, and it is the one finding I overturn.

The pre-existing missing-tag problem is real (see below) but it is only *reachable* through a full dependency-graph re-resolution. This branch triggered that re-resolution unnecessarily, because its hand-edited `Cargo.lock` was stale in a way neither report isolated:

**`Cargo.lock` still recorded `navigator 2.0.0` while `Cargo.toml` had been bumped to `2.1.0`.**

That single mismatch invalidates the lock's root package. Cargo therefore cannot reuse the locked dependency set for the workspace member, re-resolves its dependencies from scratch, and in doing so queries the real git source for `bm25` — whose tag does not exist. Hence the misleading error naming `bm25`, a crate this change never touched.

With the lock's root version synced, the four remaining `[patch]` entries stay locked, Cargo takes its patch short-circuit for them (a locked dependency with exactly one matching patch is never queried against its origin source), and no `bm25`/`facetquery`/`clikit`/`logkit` tag is ever fetched.

## Verification of the three dispatched findings

Toolchain: `cargo 1.98.0` / `rustc 1.98.0`, matching `mise.toml`'s `rust = "1.98.0"` pin. The provisioned `language-tools` CLI is not present on this machine (`command -v language-tools`, `/home/bits/.local/bin`, `/home/bits/.local/share/mise/shims` all negative), matching the test-engineer's note; ran `cargo` directly, which is what that CLI's Rust `build`/`test` gate wraps. The failure and the fix both sit at Cargo's dependency-resolution stage, upstream of anything `language-tools` adds.

Baseline method note: `git worktree add` from the primary checkout is blocked by this environment's worktree gate, and the pre-existing `land-main` worktree is broken (`fatal: not a git repository`). I therefore took the baseline inside this same worktree via `git checkout bbe368b -- Cargo.toml Cargo.lock`, then restored with `git checkout HEAD -- Cargo.toml Cargo.lock`. `src/` and `tests/` are byte-identical between `bbe368b` and `414d8c8`, so this reproduces `main`'s build state exactly.

### Finding 1 — pre-existing full-re-resolve wall: CONFIRMED, and unchanged by this branch

`cargo update` (full re-resolve) fails identically on `main`'s manifest and on this branch, before and after my fix:

```
fatal: couldn't find remote ref refs/tags/rust/bm25/v0.1.0
```

Real and general: a full re-resolution needs every patched git dependency's own origin ref fetchable, even when a local-path `[patch]` fully overrides it. Pre-existing on `main`, not introduced here, and **not fixed here** — deliberately. It remains exactly as latent as it is on `main`.

### Finding 2 — libgit2 vs. the `github-johnrichter` SSH alias: CONFIRMED as a sandbox artifact

This machine's global gitconfig rewrites `https://github.com/johnrichter/` → `git@github-johnrichter:johnrichter/` (`git config --global --get-regexp insteadof`). libgit2's built-in transport cannot resolve that alias as a hostname:

```
failed to resolve address for github-johnrichter: Name or service not known; class=Net (12)
```

`CARGO_NET_GIT_FETCH_WITH_CLI=true` switches to the system `git` binary, which has the matching SSH `Host` alias, and gets past it. Correctly diagnosed by both prior reports, correctly kept separate from finding 1. Environment config, not a repo defect.

One genuine consequence worth flagging (see plan feedback): after this cutover the graph contains a git source Cargo must actually touch on a cold cache, where before it touched none. So any dev box carrying those `insteadOf` rewrites now needs CLI-mode fetch for a cold-cache build. Warm-cache builds need no network at all (`cargo build --locked --offline` passes).

### Finding 3 — `cargo build --locked` regression: CONFIRMED, then FIXED

Baseline, `main`'s `Cargo.toml`/`Cargo.lock` at `bbe368b`:
```
$ cargo build --locked            -> exit 0
$ cargo build --locked --offline  -> exit 0   (zero network)
```

This branch as committed at `414d8c8`:
```
$ CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --locked   -> exit 101
    Updating git repository `https://github.com/johnrichter/claude-shared-tooling.git`
fatal: couldn't find remote ref refs/tags/rust/bm25/v0.1.0
error: failed to get `bm25` as a dependency of package `navigator v2.1.0 (...)`
```

A real regression against `main`, exactly as dispatched. Note the error text itself contains the clue both reports missed: cargo reports the parent as `navigator v2.1.0` (from the manifest) while the lock it was handed says `2.0.0`.

## Findings

### Blocking (fixed)

**B1. `Cargo.lock:457` — root package version not synced with the manifest bump.**
`Cargo.toml` went `2.0.0` → `2.1.0`; the lock's `navigator` entry stayed at `2.0.0`. This is the sole cause of the `--locked` regression. A version bump is not a manifest-only edit — the lock records the root package's own version and must move with it.

Isolation proof: syncing only this line, leaving the implementer's hand-written `source =` string exactly as committed, takes `cargo build --locked` from exit 101 to exit 0, compiling `frontmatter` from the git tag. So B1 alone is the blocker, and B2 below is independent of it.

### Major (fixed)

**B2. `Cargo.lock:296` — hand-written `source` string is not in Cargo's canonical form.**
The committed value used raw slashes in the tag path:
```
source = "git+https://github.com/johnrichter/claude-shared-tooling.git?tag=rust/frontmatter/v0.1.0#0a265b19..."
```
Cargo parses that, but the form it *serialises* percent-encodes the slashes:
```
source = "git+https://github.com/johnrichter/claude-shared-tooling.git?tag=rust%2Ffrontmatter%2Fv0.1.0#0a265b19..."
```
Consequence: `--locked` tolerates the raw form, but the first ordinary (non-`--locked`) cargo command anyone runs silently rewrites the line, leaving a dirty lockfile — spurious diffs for the next developer and dirty-tree failures in any CI step that checks for them. Caught by running `cargo build` without `--locked` and diffing; the fix is Cargo's own output, verified byte-stable across a subsequent plain build.

This is the concrete cost of hand-editing a lockfile rather than having cargo write it. Both defects are of that class.

### Minor (no action)

**M1. Neither prior report ran the one diagnostic that separates the two failure modes.** Both reproduced the failure and both compared branch-vs-`main`, which is good work. What was missing was varying *this branch's own inputs*: reverting or syncing the version bump alone would have re-attributed the failure immediately. The general lesson: when a failure names a component your change never touched, suspect your change's own consistency before accepting an out-of-scope root cause. Worth folding into the test-engineer's method — an "attribute the failure by bisecting your own diff" step, not just "reproduce it on both sides".

### Comment-block edits — ACCEPTED as written, no changes

The implementer flagged these for judgement. Both are accurate, proportionate, and in scope. Leaving them would have been actively wrong: the pre-change text asserted "none has cut its first tag yet" and enumerated `frontmatter` among the untagged crates. Each edit is wording-only, alters only `frontmatter`-specific claims, and leaves the other four crates' claims intact.

I specifically checked the surviving "The three reused Rust libraries ... plus the fleet's CLI output contract" opener against the five crates it precedes, since the arithmetic looks wrong at a glance. It is correct: the three are `bm25`/`frontmatter`/`facetquery` ("pure ranking/parsing/query logic", matching the parenthetical exactly), and `clikit` + `logkit` together are the CLI output contract. No finding.

I deliberately made **no** cosmetic edits to these blocks. The diff surface on a release-bearing manifest should stay minimal and auditable, and there is no accuracy defect to fix.

## Diff correctness (independent confirmation)

- `[patch]` table: only `frontmatter = { path = ... }` removed. `git diff bbe368b HEAD -- Cargo.toml` filtered to `+`/`-` lines yields **zero** lines matching `bm25|facetquery|clikit|logkit = { path`. Byte-for-byte untouched.
- `frontmatter`'s `[dependencies]` entry was already the correct tagged-git-dep shape and is unchanged by the diff — same URL and key names as the other four, only the tag path differs.
- Version: `2.0.0` → `2.1.0` in `Cargo.toml` is the only `version` change in the original diff. My fix adds the required matching change on the lock side.
- Tag hygiene: `git tag -l` shows only the pre-existing `v2.0.0` and an unrelated `backup/sample-value-cleanup/...`. No new tag.
- Push hygiene: `git ls-remote --heads origin` has no `chore/frontmatter-tag-cutover`. Nothing pushed.

## Fixes applied

One commit, `Cargo.lock` only, two lines:

1. `navigator` `version = "2.0.0"` → `"2.1.0"` (B1).
2. `frontmatter` `source` tag path percent-encoded to Cargo's canonical form (B2).

Both values are Cargo's own output, not hand-authored: I let cargo write the lock, then confirmed the result is byte-stable and that `--locked` accepts it. No change to `Cargo.toml`, `src/`, or `tests/`.

## Re-verification

All on this branch with the fix applied, warm cache unless noted:

| Command | Result |
|---|---|
| `cargo build --locked` | exit 0 — compiles `frontmatter v0.1.0 (https://...?tag=rust%2Ffrontmatter%2Fv0.1.0#0a265b19)` |
| `cargo test --locked` | exit 0 — 275 passed / 0 failed across 6 targets (244 + 3 + 5 + 8 + 9 + 6) |
| `cargo clippy --locked --all-targets` | exit 0 — zero warnings (crate sets `clippy::all` + `clippy::pedantic` at warn, `unsafe_code = "forbid"`) |
| `cargo build --locked --offline` | exit 0 — zero network on a warm cache |
| `cargo build --locked`, **cold git cache** (throwaway `CARGO_HOME`, git cache deleted, registry warmed) | exit 0 — fetches the shared-tooling repo **once** for the real frontmatter tag; **never** attempts `bm25`/`facetquery`/`clikit`/`logkit` tag fetches |
| `cargo build` (no `--locked`) then `md5sum Cargo.lock` | unchanged — lock is byte-stable, no churn |
| `cargo tree --locked -p frontmatter` | `frontmatter` from the git tag; its own `facetquery` dep correctly resolves through the workspace `[patch]` to the local checkout |

The cold-cache run is the decisive one: it is the CI-realistic case and it proves the pre-existing missing-tag wall is genuinely unreachable from `--locked` builds now, not merely masked by a warm local cache.

Also re-confirmed after the fix: `cargo update` still fails on `refs/tags/rust/bm25/v0.1.0`, identically to `main`. Exact parity — this branch neither worsens nor repairs that, which is the correct scope.

All verification scratch artifacts (throwaway `CARGO_HOME`, temp target dirs, logs) were removed. Working tree contains only the intended changes.

## Test-suite assessment

Adequate for what it covers, with one method gap.

- The crate's own 275 tests pass and are unaffected by this change — correct, this is a dependency-graph change with no behavioural surface.
- The right verification form was chosen (build reproduction across branch and baseline, exact error-text reading) and the branch-vs-`main` comparison was genuinely useful.
- Gap, per M1: no diagnostic varied this branch's own inputs, which is why a fixable in-scope defect was written up as an out-of-scope blocker. For a `Cargo.toml`/`Cargo.lock` change the standing check should be **`cargo build --locked` must pass, and a plain `cargo build` must leave `Cargo.lock` byte-unchanged**. The second half of that would have caught B2, which no one caught.

## Residual risk

- **Unchanged pre-existing risk (accepted).** `cargo update`, `cargo generate-lockfile`, deleting `Cargo.lock`, or adding/removing any dependency still forces a full re-resolve and still fails on `refs/tags/rust/bm25/v0.1.0`. Identical to `main`. Anyone needing a re-resolve is blocked until the other four crates are tagged — but nobody needs one to build, test, or lint.
- **Cold-cache builds now need network and, on boxes with the `insteadOf` rewrites, CLI-mode git fetch.** Inherent to depending on a real tag; the tag exists and fetches cleanly.
- **Lockfile hand-editing remains fragile.** Two of two hand-edits here were defective. The next cutover should have cargo write the lock (temporarily pinning the other four to a fetchable `rev` if needed) rather than hand-editing, or at minimum run the byte-stability check above.

## Plan feedback

1. **The plan's incremental "one crate at a time" cutover is viable** — contrary to both prior reports' reconciliation notes. It does not require tagging all five simultaneously. What it requires is that each cutover leave `Cargo.lock` fully consistent with the manifest, including the root version bump and canonical source encoding. Please correct that conclusion before scheduling the `bm25`/`facetquery`/`clikit`/`logkit` tasks; as written it would have blocked three further tasks on a non-blocker.
2. **Add to each remaining cutover's acceptance:** `cargo build --locked` passes, and a plain `cargo build` leaves `Cargo.lock` byte-unchanged. Both defects here die at that gate.
3. **The `cargo update` wall is still worth fixing on its own merits**, just not as a blocker for this branch. Tagging the other four is the clean fix; pinning them to a fetchable `rev =` on a real commit is the cheaper interim one. Orchestrator's call.
4. **Consider `net.git-fetch-with-cli = true` in a repo `.cargo/config.toml`** so cold-cache builds work on boxes carrying the `insteadOf` rewrites without per-invocation env vars. This changes fetch behaviour for every contributor, so it is a policy decision, not a review fix — flagging, not applying.
5. **Version bump is correct as a minor.** `2.1.0` for a dependency-source repin with no API change fits the repo's convention.

## Files touched by this review

- `/home/bits/Development/workspaces/psa-platform/workspace-tools/.claude/worktrees/frontmatter-tag-cutover/Cargo.lock` — the two-line fix
- `/home/bits/Development/workspaces/psa-platform/workspace-tools/.claude/worktrees/frontmatter-tag-cutover/.task-reports/frontmatter-tag-cutover-quality-review.md` — this report

Nothing tagged, nothing pushed, no branch or merge operations performed.
