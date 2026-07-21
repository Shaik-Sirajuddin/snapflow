# Project rules for Claude / AI agents

## Commits: no Claude co-author

- **Never** add `Co-Authored-By: Claude...`, `Co-Authored-By: ...@anthropic.com`, or `Claude-Session:` trailers to commit messages.
- **Never** set Claude (or any Anthropic bot identity) as author or committer.
- Commits in this repo must be authored only by the human maintainer.
- If your tool auto-appends co-author trailers, disable that behavior for this project.

Local enforcement: enable the shared hook with:

```bash
git config core.hooksPath .githooks
```

CI enforces the same rule via `.github/workflows/no-claude-coauthor.yml`.
