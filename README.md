# workspace-tools

The navigator frontmatter/tag-aware discovery CLI.

## Claude Code setup

This repo's `.claude/settings.json` enables plugins from the `jr-claude-plugins` marketplace. Register it once at the Claude user level — repo settings carry no machine-specific paths:

```sh
claude plugin marketplace add git@github.com:johnrichter/claude-marketplace.git
# or, with the psa-platform repos checked out as siblings:
claude plugin marketplace add ../marketplace-public
```

Knowledge bases are configured at the Claude user level, not per repo.
