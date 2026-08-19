# workspace-tools

> **Purpose:** The navigator frontmatter/tag-aware discovery CLI.

One of the sibling repos under `psa-platform`. Worked on directly — not through the PSA `workspace`. Outside `workspace`, Claude has no PSA identity; only the general conventions below apply.

## Conventions come from plugins, not this file

Authoring & output style, the frontmatter + tagging schema, git governance, code-authoring standards, model tiering, and writing voice are all provided by the enabled Claude Code plugins (`.claude/settings.json`). The `framework-conventions` plugin injects the full convention set every session via its SessionStart hook — never restate or `@import` it here.

## Workspace boundary

This repo root is the working boundary. Don't create, modify, or delete files outside it unless explicitly instructed.

## Projects

Plan/build-with-team work lives in `.dat/<slug>/` at the repo root, tracked in git so `delivery-agent-team` can plan, build, and resume in-repo. Keep efforts short-lived: complete, cut a release, then prune. `.dat/` will be retired in favor of `.anoikis/` (reserved now) as anoikis-based planning takes over.

## Discovery

`navigator` is enabled for frontmatter/tag-aware discovery over this repo and its `additionalDirectories` — prefer it over raw grep/glob. Vocabulary config lives in `navigator.toml`.
