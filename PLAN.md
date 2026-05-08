# Build Plan — gloss

This file is the entry point for any new Claude Code / agent session
opening this directory. Read `README.md` first for the product pitch
and design constraints, then this file for the execution plan.

## Where the core came from

The gloss engine was first built inside the YiYi project at:

```
/Users/Four/PersonalProjects/YiYi/app/src-tauri/src/engine/checkpoint.rs
```

It is battle-tested by 13 unit tests (all passing) and 1 integration
test. The original wraps it with:

- per-session refs (`refs/yiyi/<session>/<turn>__<phase>`)
- pre-turn / post-turn phase semantics
- a tokio-task-local `with_session_id` plumbing
- Tauri commands for the desktop app

For gloss we **strip those YiYi-specific concepts** and end up
with a flat per-workspace timeline. `src/checkpoint.rs` here is the
already-stripped version — read it before changing anything else.

## Milestones

### M1 — Library crate (✅ scaffolded, see `src/checkpoint.rs`)

**Goal**: `cargo test` green; library exposes `snapshot`, `restore`,
`list`, `diff`, `gc_stale_refs`.

**To-do**:
- [ ] `cargo check` clean (verify after first edit)
- [ ] `cargo test` passes ported tests in `tests/checkpoint.rs`
- [ ] Public API in `src/lib.rs` re-exports every type a caller needs

### M2 — CLI (`gloss snap | list | restore | gc`)

**Goal**: human can drive the engine from a terminal, no daemon yet.

```
gloss snap [label]            # one-shot snapshot of cwd
gloss list                    # print timeline (id | when | label | files)
gloss restore <id>            # restore cwd to checkpoint <id>
gloss diff <id>               # print unified diff
gloss gc [--days 30]          # prune stale refs
```

**Implementation**: add `clap` for argument parsing in `src/main.rs`.
All commands take `cwd` as the workspace by default, `--workspace
<path>` to override.

### M3 — fs-watching daemon (`gloss serve`)

**Goal**: leave it running in a terminal, it auto-snapshots whenever
files change.

**Design**:
- Use the `notify` crate (recommended watcher backend per platform).
- Maintain an in-memory "dirty set" of changed paths.
- Debounce: when no event has arrived for **2 seconds**, take an
  incremental snapshot using the dirty set as `dirty_paths`.
- Pause snapshotting if the workspace has been quiet for >24h.
- Honor `EXCLUDE_DIRS` at the watcher level (don't even subscribe to
  `node_modules/**`).

**Edge cases to think through**:
- File rename: `notify` reports remove+create; debounce should coalesce.
- Massive churn (`npm install`): the `EXCLUDE_DIRS` filter must catch
  this, otherwise the daemon will livelock. **Verify before trusting.**
- Workspace deleted: daemon should exit cleanly, not loop.

### M4 — Web UI (`gloss serve` exposes `:7423`)

**Goal**: open the URL, see timeline + diff + restore button.

**Decision pending — pick before writing UI**:
- Option A: vanilla HTML + a tiny bit of JS, axum server-rendering
- Option B: HTMX + axum (recommended — best fit for this scope)
- Option C: React SPA + axum API (more work, nicer interactions)

The user leaned toward B but didn't lock it in — confirm with them
before scaffolding the UI.

**Routes (HTMX flavor)**:
- `GET /` — timeline page (server-rendered list of checkpoints)
- `GET /checkpoint/:id` — fragment: diff view for one checkpoint
- `POST /checkpoint/:id/restore` — perform restore, redirect to `/`
- `GET /sse` — server-sent events stream so newly-created checkpoints
  appear in the timeline live without a refresh

**Embedding**: use `rust-embed` or `include_str!` so the binary ships
with all assets baked in. No `dist/` directory to deploy.

### M5 — MCP server (`gloss mcp`)

**Goal**: Claude Code can semantically label its own checkpoints.

**Tools to expose**:
- `checkpoint(label: string) -> { id, files_changed, ... }`
- `list_checkpoints(limit?: number) -> Checkpoint[]`
- `restore(id: string) -> { restored_files, removed_files, stash_id? }`
- `diff(id: string) -> FileDiff[]`

**Implementation**: use the `rmcp` crate (Anthropic's official Rust MCP
SDK) or hand-roll stdio JSON-RPC if `rmcp` isn't ready. Each tool call
short-circuits to the same library functions M1 exposed.

### M6 — Dogfood

**Goal**: the maintainer (Four) installs `gloss mcp` into their
own Claude Code config and uses it for a real coding week.

**Measurable**: at the end of the week, can answer "did you actually
hit the restore button at least once for a real reason?" If yes, ship
v0.1. If no, this whole project was a misread of the pain.

## Decisions still open (ask the user before resolving)

- **Web UI stack** (A / B / C above) — currently B is recommended.
- **CLI binary name** — `gloss` is what the README assumes, but
  could be `sg` for ergonomics.
- **Default port** — README says 7423; collision-resistant or memorable?
- **License** — MIT vs Apache-2.0.

## Constraints

- Single binary, no runtime deps beyond what's compiled in. `git2` is
  already vendored (libgit2 statically linked).
- Cross-platform: macOS / Linux / Windows must all work. `notify` and
  `git2` already do; the web UI is OS-agnostic. Test on macOS first
  (the maintainer's platform), then make sure CI covers Linux.
- No telemetry, no phone-home, no network calls. Everything local.

## What is NOT in scope (V1)

- Compaction / packing of loose objects (libgit2 doesn't expose `git gc`
  cleanly; either shell out to `git gc` if available, or accept loose-
  object growth and revisit in V2).
- `.gitignore` integration. EXCLUDE_DIRS is the only exclusion mechanism.
- Multiple workspaces in one daemon. One `gloss serve` per repo.
- Authentication / multi-user.
- Sync between machines.

## Style

- Standard `rustfmt`. No custom config.
- Errors as `String` for now (matches the original YiYi code). Migrate
  to `thiserror`-based enum if/when it becomes painful.
- Tests use `serial_test` because they touch the home-dir / fs root via
  the `GLOSS_HOME` env var.
- Comments only when the WHY is non-obvious. No commentary on what the
  code does — well-named functions handle that.
