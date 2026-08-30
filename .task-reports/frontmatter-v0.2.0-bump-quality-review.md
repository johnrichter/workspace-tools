---
name: "frontmatter v0.2.0 bump — quality review"
description: "Final quality review of the frontmatter v0.1.0 -> v0.2.0 dependency bump, the workspace-tools v2.2.0 release cut, and the governance-workspace-structure repin."
id: "doc:workspace-tools:frontmatter-v0.2.0-bump-quality-review"
tags: [type:doc, topic:process, status:complete, privacy:public, owner:public]
links: []
updated: 2026-08-30T00:00:00Z
---

# frontmatter v0.1.0 -> v0.2.0 — quality review

**Verdict: FIX-APPLIED.** The bump is real, scoped, and safe, but the hand-edited
`Cargo.lock` recorded the **annotated tag object's** SHA where cargo records the **peeled
commit** SHA. Both prior reports asserted PASS on exactly this point. Fixed and re-verified.

## Blocking finding (fixed)

### F1 — `Cargo.lock:296`: lockfile pinned the tag object, not the commit

`rust/frontmatter/v0.2.0` is a signed **annotated** tag, so `git ls-remote --tags` emits two
rows:

```
4c7e098b2b315905a6753086b943fcff53d5c607	refs/tags/rust/frontmatter/v0.2.0        <- tag object
12198e08baea4fdede3cbbf1785aa143c0ef5233	refs/tags/rust/frontmatter/v0.2.0^{}     <- peeled commit
```

The hand-edit took the first row. Cargo's git-source fragment is always a **commit** OID:
cargo resolves `GitReference::Tag` by peeling the ref to `ObjectType::Commit`. Evidence,
independent of that knowledge:

- **Same lockfile, previous entry.** `v0.1.0` is also annotated — tag object `061a3296`,
  peeled commit `0a265b19` — and `main`'s entry records `0a265b19`, the peeled commit.
- **Cargo's own behavior on this machine.** `~/.cargo/git/checkouts/claude-shared-tooling-*/`
  holds a `12198e0/` checkout created 14:54 by cargo's *own* resolution of the v0.2.0 tag,
  before the lock was hand-edited. The later `4c7e098/` checkout (15:09) has
  `HEAD = 12198e08` — cargo peeled the value it was handed and created a second,
  misnamed checkout of the same commit.

Why the green suite did not catch it: cargo tolerates a tag object as a `precise` rev and
peels it, and the two checkouts are byte-identical (`diff -rq` over
`rust/frontmatter/`: no differences). So the build compiled the correct source and every
test passed — a green-but-hollow signal. The defect is in the lockfile's meaning, not the
bytes built: the recorded value is not what cargo resolves the pin to, so any successful
re-resolution rewrites it, and the file misstates the commit it claims to lock.

**Fix applied:** fragment set to `12198e08baea4fdede3cbbf1785aa143c0ef5233`. After the fix
`cargo build --locked` reports
`Compiling frontmatter v0.1.0 (...tag=rust%2Ffrontmatter%2Fv0.2.0#12198e08)`.

**Rule for future repins of a tag pin:** take the `^{}` row from `git ls-remote --tags`, not
the bare tag row. `git rev-parse <tag>^{commit}` is the unambiguous form.

## Verification performed

| Check | Result |
|---|---|
| `git diff main -- Cargo.toml Cargo.lock` scope | One line each, tag + fragment only. No stray edits. |
| Remote tag authenticity | `git ls-remote` confirms both tag object and peeled commit (see F1). |
| `cargo build --locked` | Pass, fresh (`cargo clean -p frontmatter -p navigator` first). |
| `cargo test --locked` | 275 pass / 0 fail (244 unit, 3 adversarial, 5 adversarial_extra, 8 cli, 9 help, 6 parity). |
| `cargo clippy --locked --all-targets` | Clean, no warnings. |
| `cargo update -p frontmatter --dry-run` | Fails on `facetquery`'s nonexistent tag — hand-edit rationale independently confirmed. |
| Upstream tag-to-tag diff | Read directly (see below), not taken from the prior summary. |

## Upstream diff v0.1.0..v0.2.0 — read directly

One commit touches the crate: `4e9ef40` "C2: add at_most_one cardinality, close the
owner-tag false-failure gap". But the change is **larger than the four `.rs` files both
prior reports described** — the crate `include_str!`s two schema files from outside its own
directory (`rust/frontmatter/src/profile.rs:37,2108`), so they ship inside the binary:

- `schemas/frontmatter/frontmatter-core.schema.json` — adds the `at_most_one` cardinality
  mechanism, adds the `at_most_one_cardinality` cascade step, and **rewords** the
  `MULTIPLE_SINGLE_VALUE_TAGS` code's `meaning`/`source` prose.
- `schemas/frontmatter/frontmatter-profile.meta.schema.json` — widens the `cardinality`
  enum to include `at_most_one` (strictly more permissive).

Neither report mentioned these. They are the files that make the new phase *live* rather
than dead code, so omitting them left the safety argument incomplete. Having read them, the
conclusion still holds — but for a reason neither report stated:

- `Cardinality` is `pub(crate)`, so the new `AtMostOne` variant cannot break any external
  exhaustive match.
- `at_most_one_cardinality_phase` early-returns unless the core profile declares the cascade
  step (it now does), then iterates only namespaces whose cardinality is `AtMostOne`.
- **The bundled packs did not change.** `frontmatter-default.pack.json` and
  `frontmatter-reports.pack.json` are byte-identical between tags, and the default pack
  declares only `type`/`status` (singleton) and `topic` (at_least_one). Despite the commit
  title, no namespace anywhere upstream is `at_most_one` yet.
- This repo declares no `at_most_one` either (`grep` over `src/`, `tests/`: only singleton /
  at_least_one / optional), and no test or doc pins the reworded `MULTIPLE_SINGLE_VALUE_TAGS`
  prose. `src/lint.rs:613` asserts that code, but for a singleton case whose behavior is
  unchanged.

**Net:** `navigator lint`/`find`/`search` output over any existing pack is unchanged. The
change is a pure capability addition — see the release-version reasoning below.

## Release: why v2.2.0, not the suggested v2.1.1

`Cargo.toml`'s version is consumer-visible: `src/main.rs:466` feeds `CARGO_PKG_VERSION` into
every record's `service_version`, so it must move with the tag. Bumped `2.1.0 -> 2.2.0` in
`Cargo.toml` and `Cargo.lock` (no test pinned the old string; build/test/clippy re-run green
at 2.2.0).

**Minor, not patch.** The dispatch suggested a patch bump on the premise of "no
consumer-visible behavior change". That premise is not quite right: a pack author can now
declare `cardinality: at_most_one` and have navigator accept and enforce it, where v2.1.0
rejected that pack outright (unknown enum value at both meta-schema and serde layers). New,
backward-compatible functionality is a minor bump.

The decisive precedent is this repo's own last release. `v2.0.0 -> v2.1.0` was cut **minor**
for the structurally identical case, as the plugin CHANGELOG 0.5.0 entry records: a
frontmatter bump adding an allowed-value check that "stays inert until a pack populates
`allowed_values`". Same shape — inert-until-declared new pack capability — so the same
increment. Cutting this one patch would contradict that precedent.

## Plan feedback — the dispatch's digest instruction is wrong for this plugin

The dispatch directed me to rebuild `governance-workspace-structure`'s digests from
"extracted-binary hashes, NOT archive-file hashes, a defect a prior review caught exactly
here". **That rule belongs to `governance-git`, not this plugin.** Following it here would
have broken provisioning for every user. The two plugins verify at different layers:

| | `governance-git` (git-tools) | `governance-workspace-structure` (navigator) |
|---|---|---|
| Digest subject | the **extracted binary** | the **archive** (`.tar.gz`) |
| Verified when | after extraction, before install; re-verified pre-exec | **before** extraction, so tampered bytes never reach disk (`hooks/bootstrap.sh:344-349`) |
| Published source | `binary-checksums.txt` | `checksums.txt` |

`data/binary-digests.json`'s own schema description states this explicitly: "the archive
digest the release records in its checksums.txt, **NOT** a digest of the binary inside."
And navigator's release workflow publishes only archive checksums
(`.github/workflows/release.yml:171`, `sha256sum ./*.tar.gz`) — no binary-checksums.txt
exists to copy from. The extracted binary's hash is computed
(`hooks/bootstrap.sh:352`) only as a local `.sha256` sidecar for re-verification from disk;
it is deliberately not the tracked trust anchor.

Repin therefore used **archive** digests copied verbatim from the published `checksums.txt`,
per this plugin's documented contract. Orchestrator/product-architect: the "extracted-binary
digests" rule should be scoped to governance-git in whatever guidance carried it forward,
or the next repin will regress provisioning.

## Test-suite assessment

Adequate for what it covers, with one real gap.

- The existing 275-test suite is the right regression net for a dependency bump; no new test
  surface is warranted for a pin change.
- **Gap: nothing verifies the lockfile pin itself.** F1 is precisely the class of defect this
  suite cannot see — the pin can name the wrong object and every test still passes, because
  cargo silently peels it. `cargo build --locked` is not a check that the fragment equals
  `git rev-parse <tag>^{commit}`.
  Suggested closure (cheap, catches this permanently): a coherence check asserting that for
  every git-source row in `Cargo.lock`, the fragment equals the peeled commit of the `tag=`
  query parameter. This mirrors what `release/coherence_test.sh` already does for the
  plugin's tag loci.
- The test-engineer's "hash matches the remote tag" check was circular: it compared against
  `ls-remote`'s unpeeled row without noticing the `^{}` row directly beneath it. Verifying a
  hash against the same command that produced it is not independent confirmation.
- The upstream-diff review was scoped to `rust/frontmatter/` and so missed the two
  `include_str!`-embedded schema files. For a dependency whose behavior is schema-driven, the
  diff scope must be the whole upstream repo, filtered to what the crate embeds.

## Residual risk

- **Pre-existing, orthogonal:** `cargo update` / non-`--locked` `cargo build` remain broken
  while `bm25`/`facetquery`/`clikit`/`logkit` are declared on git tags that do not exist and
  are bridged by `[patch]`. Every future pin bump in this repo will need the same hand-edit,
  which is exactly how F1 arose. Recommend a follow-up task to either tag those four crates
  or move them to `[patch]`-only declarations, removing hand-editing from the loop.
- The release workflow checks out the `ai-shared-lib` sibling at default-branch HEAD with no
  pinned ref (`release.yml:114-119`). Not triggered by this task (frontmatter now resolves
  from its own tag, not the patch bridge), but the four still-patched crates make release
  builds non-reproducible against a moving sibling. Flagged, not fixed — out of scope.
- The reworded `MULTIPLE_SINGLE_VALUE_TAGS` `meaning`/`source` strings ship in the binary's
  embedded core schema. Nothing in this repo surfaces or pins them, but a downstream consumer
  reading `embedded_core_json()` for that prose would observe the new wording.

## Release chain executed

| Step | Outcome |
|---|---|
| Merge `chore/frontmatter-v0.2.0-bump` -> `main` (`git-tools merge`) | `4164461`, already-signed |
| `git-tools push main` | `b380e5d` -> `4164461` |
| `git-tools tag create 2.2.0 --shape vX.Y.Z` | tag `v2.2.0` signed, created, pushed |
| CI on `main` and on tag `v2.2.0` | both success |
| SC-DISTRIBUTION release workflow | success (3m23s); four archives + `checksums.txt` published, not a draft |
| Published-asset verification | all four archives re-downloaded, `sha256sum -c checksums.txt` OK; `linux_arm64` binary runs and reports `navigator 2.2.0` |

CI built the release with `cargo build --locked` on a cold cargo git cache, which independently confirms the corrected lockfile fragment resolves from a clean checkout — not just from this machine's warm cache.

### governance-workspace-structure repin (marketplace repo)

Done in worktree `.claude/worktrees/repin-navigator-v2.2.0`, plugin `0.5.0 -> 0.6.0`, merged as `4b01af1` and pushed to marketplace `main`.

Loci moved (the first three are asserted to agree by `release/coherence_test.sh`):

- `hooks/bootstrap.sh` — `NAVIGATOR_TAG="v2.2.0"`
- `data/binary-digests.json` — tag plus all four per-(os,arch) rows, rekeyed to `navigator_2.2.0_*`
- `navigator.toml` (repo root) — `navigator_version = "2.2.0"`
- `.claude-plugin/plugin.json`, `CHANGELOG.md`, `README.md`

Digests are **archive** sha256 values copied verbatim from the published `checksums.txt`, per this plugin's contract — see the plan-feedback section above for why the dispatch's extracted-binary instruction was not followed.

Verification beyond the unit suites, since a green unit suite cannot prove a pin reaches a real artifact:

- **End-to-end provisioning against the real release.** Ran `hooks/bootstrap.sh` with a scratch `CLAUDE_PLUGIN_DATA`/`WORKSPACE_TOOLS_DATA_HOME` and no base-URL override, so it fetched from GitHub: archive downloaded, matched the pinned digest, extracted, installed `navigator-2.2.0` with its sidecar, exported `WORKSPACE_TOOLS_BIN`. This is the producer↔consumer check that matters for a repin.
- **Output parity.** `navigator lint` over the whole marketplace repo (1035 files scanned) is byte-for-byte identical under 2.1.0 and 2.2.0 once `service_version` is excluded — empirically confirming the "nothing declares `at_most_one` yet, so results are unchanged" claim rather than asserting it. Pre-existing rollup under both binaries: 993 valid, 1 invalid, 41 missing frontmatter.
- **Old pin cross-check.** The retired `v2.1.0` `linux_arm64` row matches the sha256 of the real published 2.1.0 archive, independently confirming that this table has always pinned archive digests.
- **Full plugin suite:** all 13 `*_test.sh` scripts pass, 0 failures.

### Environment finding — the plugin suite is cwd-sensitive under this sandbox

`hooks/bootstrap_test.sh` fails 19 cases when run with a cwd outside `/tmp`, and passes with cwd in `/tmp`. **This is pre-existing and not caused by the repin:** unmodified `main` content fails the same 19 cases from the same cwd, and the repinned content passes all cases from `/tmp`.

Root cause is a sandbox artifact, not a plugin defect. Traced with `sh -x`: inside `digest_for` (`hooks/bootstrap.sh:230-238`) the `jq` invocation is replaced by `true` and returns empty, so every digest lookup misses and bootstrap reports "no pinned digest". `jq` itself works correctly from the same cwd when invoked directly, and PATH, tool resolution, and environment are byte-identical between the passing and failing cwds. Reduced to a minimal reproduction independent of the test suite.

Actionable for whoever owns CI for this plugin: pin the suite's cwd, or invoke `jq` by absolute path in `digest_for`, so the result does not depend on where the runner happens to stand. Worth confirming the same artifact is not silently affecting other plugins' suites.

## Files changed by this review

- `Cargo.lock` — F1 fix (git fragment -> peeled commit); navigator version -> 2.2.0.
- `Cargo.toml` — navigator version -> 2.2.0.
- `.task-reports/frontmatter-v0.2.0-bump-report.md`,
  `.task-reports/frontmatter-v0.2.0-bump-test-verification.md` — correction pointer appended
  so the next repin does not copy the unpeeled-hash pattern. Original findings left intact.
