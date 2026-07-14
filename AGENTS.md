# Rust/padjutsu

The application code is divided into modules located in the `./crates` folder:

- Crate names are prefixed with `padjutsu-`. For example, the `workspace` folder's crate is named `padjutsu-workspace`. The only exception is the daemon whose crate is called `padjutsud`.
- When using format! and you can inline variables into {}, always do that.

Run `just fmt` (in the project directory) automatically after making Rust code changes; do not ask for approval to run it. Before finalizing a change to `padjutsu`, run `just fix` to fix any linter issues in the code. Prefer scoping with `-p` to avoid slow workspace-wide Clippy builds; only run `just fix` without `-p` if you changed shared crates. Additionally, run the tests:
1. Run the test for the specific project that was changed. For example, if changes were made in `crates/padjutsu-gamepad`, run `just test -p padjutsu-gamepad`.
2. Once those pass, if any changes were made in common, core, or protocol, run the complete test suite with `just test --all-features`.
Don't ask the user before running `just fix` to finalize. `just fmt` does not require approval. project-specific or individual tests can be run without asking the user, but do ask the user before running the complete test suite.

To build the project, run `just build`.

## Issue tracking with br

Use `br` for all issue tracking. Do not create duplicate Markdown task lists.

```bash
br ready --json
br create "Issue title" --description "Detailed context" -t bug -p 1 --json
br update <issue-id> --claim --json
br close <issue-id> --reason "Completed: <proof>" --json
br sync --flush-only
```

The local SQLite database is ignored by Git. Commit `.beads/issues.jsonl` together with the code. `br` never runs Git commands or installs Git hooks.

Before ending a work session, run quality gates, update or close the issue, flush JSONL, commit, pull with rebase, and push. Work is not complete until `git push` succeeds and `git status` reports the branch is up to date.
