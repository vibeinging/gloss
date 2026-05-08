# gloss

> A standalone gloss daemon + web UI + MCP server for AI coding agents.
>
> Captures every meaningful change to your workspace into an out-of-tree git
> repo (your real `.git` is never touched). Browse the timeline, diff any
> point, and roll back — without polluting your real git history.

## Why

Heavy Claude Code / Codex / OpenCode users hit the same gaps:

1. **Cross-session rollback** — agents only remember their own session. You
   want "roll back to before the agent started 2 hours ago", not "undo the
   last 3 messages".
2. **Auto-commit without polluting `git log`** — manually `git commit` every
   5 minutes is friction; a `git stash` per turn turns history into trash.
3. **Lightweight diff viewer** — opening VS Code or `git difftool` to see
   what an agent did is overkill.
4. **One-click rollback** — see something you don't like, click, gone.

Existing IDE plugins (JetBrains Local History, VS Code Timeline) are
locked into one editor. Cline / Cursor have built-in checkpoints but only
inside their own UI. gloss is **agent-agnostic, editor-agnostic,
and runs as a single Rust binary**.

## Form factor

One binary, three modes:

```
gloss serve [path]   # daemon: fs-watch + auto-snapshot + web UI
gloss mcp            # stdio MCP server for Claude Code / others
gloss snap [label]   # manual snapshot CLI (escape hatch)
```

Typical use:

```bash
$ cd ~/code/my-project
$ gloss serve
gloss watching ~/code/my-project
web ui: http://localhost:7423
```

Browser opens `localhost:7423` showing:

- Left: timeline of checkpoints (newest first)
- Right: file diff viewer for the selected checkpoint
- Top: optional human-readable label per checkpoint (from MCP / CLI)
- Each checkpoint has a **"restore to here"** button

Configure Claude Code to talk to it via `~/.claude/mcp_settings.json`:

```json
{
  "mcpServers": {
    "gloss": { "command": "gloss", "args": ["mcp"] }
  }
}
```

Then Claude Code can call:
- `checkpoint(label)` — semantic snapshot before a risky task
- `list_checkpoints()` — see history
- `restore(id)` — roll back

## Storage layout

```
~/.gloss/
└── workspaces/
    └── <fnv1a-of-canonical-path>/
        ├── git/                # real libgit2 repo, separate from user .git
        │   └── refs/gloss/<sortable-id>
        └── .last_gc            # debounce marker for periodic GC
```

- Refs live under `refs/gloss/...` so they never collide with user
  refs. The user's `.git` is read-only from gloss's perspective.
- libgit2 content-addressed storage gives free dedup across snapshots.
- Default retention: 30 days. Stale refs are auto-pruned daily.
- `EXCLUDE_DIRS` (`.git`, `node_modules`, `target`, `__pycache__`,
  `dist`, `build`, `.next`, `.venv`, `venv`, `.tox`, `.cache`) are never
  indexed.
- Files larger than 50 MiB are skipped.

## Restore semantics

Two strategies, configurable per call:

- **Direct restore** (default for CLI / web): tool writes the workspace
  directly. Fast. If an agent is mid-task, race-condition risk — UI must
  warn the user to stop the agent first.
- **Cooperative restore** (via MCP, Phase 2): tool emits a "user wants to
  restore to X" signal; the agent reads it and exits its current task
  cleanly before applying. Safer.

Before any restore, uncommitted hand-edits in the workspace are auto-
captured to a `stash__<ts>` ref. Nothing is ever silently dropped.

## Non-goals (V1 strictly)

- Multi-user / cloud sync
- VS Code extension / IDE integration
- AI-generated diff summaries
- Mobile apps
- Authentication
- Cross-machine timeline merging

## Status

Core checkpoint engine extracted from the YiYi project (proven by 13
unit tests in the original codebase). Daemon, web UI, and MCP layers are
to-be-written. See `PLAN.md` for the step-by-step build plan.

## License

TBD (probably MIT or Apache-2.0).
