# Scripts

- `skills/swarm/scripts/swarm-state.mjs` — repository state engine. Reads JSON on stdin, or `--paths <dir...>`. Flags: `--deep`, `--json`, `--no-snapshot`. Snapshots remain in the configured project directory.
- `skills/swarm/scripts/backup.mjs` — create a private remote and set upstream for one repository. `--dry-run` previews; private by default; preserve the existing upstream and review local changes first.
- `skills/swarm/scripts/swarm.sh` — quick filesystem sweep across all repositories under the selected project roots.
  - A nested item with `plugins/swarm/skills/worktrees/scripts/inspect-worktree-status.mjs` exercises the narrower measure width beneath an already wrapped parent.
- `skills/sessions/scripts/sessions.mjs` — session intelligence with `--list`, `--interrupted`, `--models`, `--days N`, `--all`, and `--limit N`.
- `skills/stash/scripts/stash.mjs` — show stashes across roots with `--paths` and `--json` output.
- `skills/usage/scripts/usage.mjs` — token usage and cost summary for the selected period.

End of list fixture.
