# Issue tracking with br

This repository uses `br`, a local-first SQLite + JSONL issue tracker.

```bash
br ready --json
br create "Issue title" -t task -p 2 --json
br update <issue-id> --claim --json
br close <issue-id> --reason "Completed" --json
br sync --flush-only
```

The SQLite database is local and ignored by Git. Commit `.beads/issues.jsonl`
with the code so another clone can rebuild its database with
`br sync --import-only`.
