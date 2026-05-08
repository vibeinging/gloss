//! Workspace checkpoint engine — gloss over the user's worktree.
//!
//! A content-addressed git repository at
//! `~/.gloss/workspaces/<workspace_hash>/git/` whose worktree is
//! pointed at the user's workspace. Each snapshot writes one ref:
//!
//!   refs/gloss/<unix_ms>_<short_oid>
//!
//! pointing at a commit whose tree is the snapshotted workspace. The
//! user's own `.git` is never touched.
//!
//! Originally extracted from the YiYi project's
//! `engine/checkpoint.rs`, simplified to drop the per-session +
//! pre/post-phase concepts (those belong to the agent loop, not the
//! storage engine).

use git2::{IndexAddOption, Oid, Repository, RepositoryInitOptions, Signature};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Heavy / regenerable directories the checkpoint never indexes.
const EXCLUDE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    ".tox",
    ".cache",
];

/// Skip files larger than this. Big enough that PowerPoint / Excel
/// attachments survive a snapshot, small enough to refuse 100MB blobs.
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// Cap on the unified-patch text per file in `diff()`. Beyond this, the
/// patch is truncated and `truncated: true` is set so callers can show
/// "diff too large" without blowing up the IPC payload.
const MAX_PATCH_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointInfo {
    /// Sortable ref-name suffix: `<unix_ms>_<short_oid>`. Stable across
    /// processes, lex-sortable for chronological listing.
    pub id: String,
    /// Optional human-readable label (from MCP / CLI). None for daemon-
    /// auto snapshots.
    pub label: Option<String>,
    pub commit: String,
    pub parent_commit: Option<String>,
    pub created_at_ms: u64,
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RestoreReport {
    pub restored_files: Vec<String>,
    pub removed_files: Vec<String>,
    /// Auto-stash commit oid if hand-edits were captured before restore.
    pub stash_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    pub patch: String,
    pub truncated: bool,
}

// ── Path / id helpers ───────────────────────────────────────────────────

fn gloss_root() -> PathBuf {
    std::env::var("GLOSS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".gloss")
        })
}

/// Stable FNV-1a 64-bit hash of the canonical workspace path.
///
/// Hand-rolled (not `DefaultHasher`) because `DefaultHasher`'s output is
/// **not** stable across Rust toolchain releases. A toolchain bump would
/// otherwise relocate every user's checkpoint dir and orphan their entire
/// timeline. FNV-1a is a documented, reproducible algorithm — safe to
/// pin into on-disk paths.
fn workspace_id(workspace: &Path) -> String {
    let canon = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    let s = canon.to_string_lossy();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

fn repo_dir(workspace: &Path) -> PathBuf {
    gloss_root().join("workspaces").join(workspace_id(workspace))
}

const REF_PREFIX: &str = "refs/gloss/";

fn ref_name(id: &str) -> String {
    format!("{REF_PREFIX}{id}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn make_id(commit: Oid) -> String {
    let oid_str = commit.to_string();
    let short = &oid_str[..oid_str.len().min(8)];
    format!("{:013}_{short}", now_ms())
}

// ── Repo management ─────────────────────────────────────────────────────

fn open_or_init(workspace: &Path) -> Result<Repository, String> {
    let dir = repo_dir(workspace);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create repo dir: {e}"))?;
    let gitdir = dir.join("git");

    // Three layouts to handle:
    //   1. Fresh           — neither path exists; init bare at `gitdir/`.
    //   2. Bare            — `gitdir/HEAD` exists (post-fix layout).
    //   3. Legacy non-bare — `gitdir/.git/HEAD` exists (pre-fix layout that
    //      wrote a `.git` gitlink into the user's workspace via libgit2's
    //      `workdir_path` init option, polluting any project that wanted
    //      its own real git).
    //
    // Critical: never set `workdir_path` in init opts and always pass
    // `update_gitlink = false` to `set_workdir`. Both of those write the
    // gitlink file into the user's workspace.
    let head_bare = gitdir.join("HEAD");
    let head_legacy = gitdir.join(".git").join("HEAD");

    if head_bare.is_file() {
        let repo = Repository::open_bare(&gitdir)
            .map_err(|e| format!("open bare shadow repo: {e}"))?;
        repo.set_workdir(workspace, false)
            .map_err(|e| format!("set workdir: {e}"))?;
        return Ok(repo);
    }

    if head_legacy.is_file() {
        let repo = Repository::open(&gitdir)
            .map_err(|e| format!("open shadow repo: {e}"))?;
        repo.set_workdir(workspace, false)
            .map_err(|e| format!("set workdir: {e}"))?;
        return Ok(repo);
    }

    let mut opts = RepositoryInitOptions::new();
    opts.bare(true).no_reinit(false);
    let repo = Repository::init_opts(&gitdir, &opts)
        .map_err(|e| format!("init shadow repo: {e}"))?;
    repo.set_workdir(workspace, false)
        .map_err(|e| format!("set workdir: {e}"))?;
    Ok(repo)
}

fn should_skip_path(rel: &Path, workspace: &Path) -> bool {
    for comp in rel.components() {
        if let std::path::Component::Normal(name) = comp {
            let s = name.to_string_lossy();
            if EXCLUDE_DIRS.iter().any(|x| *x == s.as_ref()) {
                return true;
            }
        }
    }
    let abs = workspace.join(rel);
    if let Ok(meta) = std::fs::symlink_metadata(&abs) {
        if meta.file_type().is_symlink() {
            return true;
        }
        if meta.is_file() && meta.len() > MAX_FILE_BYTES {
            return true;
        }
    }
    false
}

// ── Commit construction ─────────────────────────────────────────────────

/// Build a commit whose tree reflects the workspace state.
///
/// `dirty_paths`:
///   - `None` → full snapshot: walk the entire workspace, hash every
///     non-excluded file. Use this when the caller can't enumerate what
///     changed (manual snapshot, daemon cold start).
///   - `Some(set)` → incremental: seed the index from `parent`'s tree,
///     re-add only the dirty paths, explicitly remove paths that were
///     deleted on disk. Avoids stat'ing 10k unchanged files on every
///     auto-snapshot. Falls back to full when parent is None or the
///     dirty set is empty.
fn build_commit(
    repo: &Repository,
    workspace: &Path,
    parent: Option<Oid>,
    message: &str,
    dirty_paths: Option<&HashSet<PathBuf>>,
) -> Result<Oid, String> {
    let mut index = repo.index().map_err(|e| format!("repo index: {e}"))?;

    let incremental = match (parent, dirty_paths) {
        (Some(p), Some(set)) if !set.is_empty() => Some((p, set)),
        _ => None,
    };

    if let Some((parent_oid, paths)) = incremental {
        let parent_tree = repo
            .find_commit(parent_oid)
            .and_then(|c| c.tree())
            .map_err(|e| format!("parent tree: {e}"))?;
        index
            .read_tree(&parent_tree)
            .map_err(|e| format!("index read_tree: {e}"))?;

        let workspace_owned = workspace.to_path_buf();
        let mut cb = |path: &Path, _matched: &[u8]| -> i32 {
            if should_skip_path(path, &workspace_owned) {
                1
            } else {
                0
            }
        };
        let pathspecs: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        index
            .add_all(pathspecs.iter(), IndexAddOption::DEFAULT, Some(&mut cb))
            .map_err(|e| format!("index add_all (incremental): {e}"))?;

        // Files deleted on disk are silently skipped by add_all; remove
        // them from the index explicitly so the new tree drops them.
        for rel in paths {
            if !workspace.join(rel).exists() {
                let _ = index.remove_path(rel);
            }
        }
    } else {
        index.clear().map_err(|e| format!("index clear: {e}"))?;
        let workspace_owned = workspace.to_path_buf();
        let mut cb = |path: &Path, _matched: &[u8]| -> i32 {
            if should_skip_path(path, &workspace_owned) {
                1
            } else {
                0
            }
        };
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, Some(&mut cb))
            .map_err(|e| format!("index add_all: {e}"))?;
    }
    index.write().map_err(|e| format!("index write: {e}"))?;

    let tree_oid = index.write_tree().map_err(|e| format!("write tree: {e}"))?;
    let tree = repo.find_tree(tree_oid).map_err(|e| format!("find tree: {e}"))?;

    let sig = Signature::now("gloss", "gloss@local")
        .map_err(|e| format!("signature: {e}"))?;

    let parents: Vec<git2::Commit> = match parent {
        Some(oid) => match repo.find_commit(oid) {
            Ok(c) => vec![c],
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

    let oid = repo
        .commit(None, &sig, &sig, message, &tree, &parent_refs)
        .map_err(|e| format!("commit: {e}"))?;
    Ok(oid)
}

fn latest_commit_oid(repo: &Repository) -> Option<Oid> {
    let mut latest: Option<(u64, Oid)> = None;
    if let Ok(refs) = repo.references_glob(&format!("{REF_PREFIX}*")) {
        for r in refs.flatten() {
            if let Some(target) = r.target() {
                if let Ok(commit) = repo.find_commit(target) {
                    let when = commit.time().seconds() as u64;
                    if latest.map_or(true, |(t, _)| when >= t) {
                        latest = Some((when, target));
                    }
                }
            }
        }
    }
    latest.map(|(_, oid)| oid)
}

// ── Public API ──────────────────────────────────────────────────────────

/// Snapshot the workspace into a new checkpoint.
///
/// `label`: optional human-readable description (e.g. from an MCP
/// `checkpoint("before refactor X")` call). Daemon-auto snapshots pass
/// `None`.
///
/// `dirty_paths`: optional set of paths the caller knows have changed
/// since the last snapshot. Enables incremental commit (much faster on
/// large workspaces). Pass `None` if you don't know.
pub async fn snapshot(
    workspace: &Path,
    label: Option<String>,
    dirty_paths: Option<HashSet<PathBuf>>,
) -> Result<CheckpointInfo, String> {
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<CheckpointInfo, String> {
        if !workspace.exists() {
            return Err(format!("workspace does not exist: {}", workspace.display()));
        }
        let repo = open_or_init(&workspace)?;
        let parent = latest_commit_oid(&repo);
        let msg = label.clone().unwrap_or_else(|| format!("snapshot @ {}", now_ms()));
        let oid = build_commit(&repo, &workspace, parent, &msg, dirty_paths.as_ref())?;

        let id = make_id(oid);
        let refname = ref_name(&id);
        repo.reference(&refname, oid, true, &msg)
            .map_err(|e| format!("write ref {refname}: {e}"))?;

        let stats = compute_stats(&repo, parent, oid);
        Ok(CheckpointInfo {
            id,
            label,
            commit: oid.to_string(),
            parent_commit: parent.map(|p| p.to_string()),
            created_at_ms: now_ms(),
            files_changed: stats.files_changed,
            insertions: stats.insertions,
            deletions: stats.deletions,
            changed_files: stats.changed_files,
        })
    })
    .await
    .map_err(|e| format!("snapshot join error: {e}"))?
}

/// List checkpoints for a workspace, newest first.
pub fn list(workspace: &Path) -> Vec<CheckpointInfo> {
    let repo = match open_or_init(workspace) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let refs = match repo.references_glob(&format!("{REF_PREFIX}*")) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<CheckpointInfo> = Vec::new();
    for r in refs.flatten() {
        let Some(name) = r.name() else { continue };
        let Some(id) = name.strip_prefix(REF_PREFIX) else { continue };
        // stash refs use a different prefix and parse-fail naturally.
        let Some((ts_str, _short)) = id.split_once('_') else { continue };
        let Ok(_ts) = ts_str.parse::<u64>() else { continue };
        let Some(target) = r.target() else { continue };
        let Ok(commit) = repo.find_commit(target) else { continue };
        let parent_oid = commit.parent_id(0).ok();
        let stats = compute_stats(&repo, parent_oid, target);
        let label = {
            let raw = commit.message().unwrap_or("");
            if raw.starts_with("snapshot @") || raw.starts_with("stash before") {
                None
            } else {
                Some(raw.trim().to_string())
            }
        };
        out.push(CheckpointInfo {
            id: id.to_string(),
            label,
            commit: target.to_string(),
            parent_commit: parent_oid.map(|o| o.to_string()),
            created_at_ms: (commit.time().seconds() as u64) * 1000,
            files_changed: stats.files_changed,
            insertions: stats.insertions,
            deletions: stats.deletions,
            changed_files: stats.changed_files,
        });
    }

    out.sort_by(|a, b| b.id.cmp(&a.id)); // newest first (lex sort on ms-prefixed id)
    out
}

/// Restore the workspace to the given checkpoint.
///
/// If `paths` is `Some`, only those paths (relative to workspace) are
/// restored. If `None`, every file the checkpoint tracks is restored,
/// and any tracked-but-not-in-checkpoint file is removed (within the
/// non-excluded subtree).
///
/// Before any change, uncommitted hand-edits are auto-stashed to a
/// `refs/gloss-stash/...` ref so the user can recover them.
pub async fn restore(
    workspace: &Path,
    id: &str,
    paths: Option<Vec<PathBuf>>,
) -> Result<RestoreReport, String> {
    let id = id.to_string();
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<RestoreReport, String> {
        let repo = open_or_init(&workspace)?;
        let refname = ref_name(&id);
        let reference = repo
            .find_reference(&refname)
            .map_err(|_| format!("checkpoint not found: {id}"))?;
        let commit_oid = reference
            .target()
            .ok_or_else(|| "ref has no target".to_string())?;
        let commit = repo
            .find_commit(commit_oid)
            .map_err(|e| format!("find commit: {e}"))?;
        let tree = commit.tree().map_err(|e| format!("commit tree: {e}"))?;

        let mut report = RestoreReport::default();
        report.stash_commit = stash_uncommitted(&repo, &workspace);

        let mut opts = git2::build::CheckoutBuilder::new();
        opts.force();
        // Never let libgit2 remove untracked: EXCLUDE_DIRS (node_modules,
        // target, ...) are untracked-by-design and must survive restore.
        // We do our own targeted removal sweep below.
        opts.remove_untracked(false);

        if let Some(ps) = &paths {
            for p in ps {
                opts.path(p);
            }
        }

        repo.checkout_tree(tree.as_object(), Some(&mut opts))
            .map_err(|e| format!("checkout_tree: {e}"))?;

        let mut tracked: HashSet<PathBuf> = HashSet::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }
            let name = match entry.name() {
                Some(n) => n,
                None => return git2::TreeWalkResult::Ok,
            };
            let rel = if dir.is_empty() {
                PathBuf::from(name)
            } else {
                PathBuf::from(dir).join(name)
            };
            tracked.insert(rel);
            git2::TreeWalkResult::Ok
        })
        .map_err(|e| format!("tree walk: {e}"))?;

        let restrict: Option<HashSet<PathBuf>> = paths.map(|v| v.into_iter().collect());

        for rel in &tracked {
            let include = match &restrict {
                Some(set) => set.contains(rel),
                None => true,
            };
            if include {
                report.restored_files.push(rel.to_string_lossy().to_string());
            }
        }

        if restrict.is_none() {
            let mut existing: Vec<PathBuf> = Vec::new();
            collect_workspace_files(&workspace, &workspace, &mut existing);
            for rel in existing {
                if !tracked.contains(&rel) {
                    let abs = workspace.join(&rel);
                    if std::fs::remove_file(&abs).is_ok() {
                        report.removed_files.push(rel.to_string_lossy().to_string());
                    }
                }
            }
        }

        Ok(report)
    })
    .await
    .map_err(|e| format!("restore join error: {e}"))?
}

/// Capture any worktree changes the user made beyond the latest
/// checkpoint into a `refs/gloss-stash/<ts>` ref. Returns the
/// stash commit oid (as hex) when a stash was actually created.
fn stash_uncommitted(repo: &Repository, workspace: &Path) -> Option<String> {
    let parent = latest_commit_oid(repo)?;

    // Cheap pre-check: no diff vs. index/HEAD → skip the full add_all.
    let mut status_opts = git2::StatusOptions::new();
    status_opts.include_untracked(true).recurse_untracked_dirs(true);
    if repo
        .statuses(Some(&mut status_opts))
        .map(|s| s.is_empty())
        .unwrap_or(false)
    {
        return None;
    }

    let msg = format!("stash before restore @ {}", now_ms());
    let oid = build_commit(repo, workspace, Some(parent), &msg, None).ok()?;
    if oid == parent {
        return None;
    }
    let refname = format!("refs/gloss-stash/{}", now_ms());
    repo.reference(&refname, oid, true, &msg).ok()?;
    Some(oid.to_string())
}

/// Per-file unified diff between a checkpoint and its parent.
pub async fn diff(workspace: &Path, id: &str) -> Result<Vec<FileDiff>, String> {
    let id = id.to_string();
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Vec<FileDiff>, String> {
        let repo = open_or_init(&workspace)?;
        let refname = ref_name(&id);
        let reference = repo
            .find_reference(&refname)
            .map_err(|_| format!("checkpoint not found: {id}"))?;
        let child = reference
            .target()
            .ok_or_else(|| "ref has no target".to_string())?;
        let parent = repo
            .find_commit(child)
            .ok()
            .and_then(|c| c.parent_id(0).ok());

        let diff = match diff_between(&repo, parent, child) {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        let mut entries: HashMap<String, FileDiff> = HashMap::new();
        let nd = diff.deltas().count();
        for i in 0..nd {
            let Ok(patch_opt) = git2::Patch::from_diff(&diff, i) else { continue };
            let Some(mut patch) = patch_opt else { continue };
            let Some(delta) = diff.get_delta(i) else { continue };
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if path.is_empty() {
                continue;
            }
            let status = match delta.status() {
                git2::Delta::Added => FileStatus::Added,
                git2::Delta::Deleted => FileStatus::Deleted,
                git2::Delta::Renamed => FileStatus::Renamed,
                git2::Delta::Copied => FileStatus::Copied,
                _ => FileStatus::Modified,
            };
            let (_, additions, deletions) = patch.line_stats().unwrap_or((0, 0, 0));
            let buf = patch.to_buf().ok();
            let raw = buf
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("");
            let (patch_str, truncated) = if raw.len() > MAX_PATCH_BYTES {
                let cut = raw
                    .char_indices()
                    .take_while(|(i, _)| *i < MAX_PATCH_BYTES)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                (raw[..cut].to_string(), true)
            } else {
                (raw.to_string(), false)
            };
            entries.insert(
                path.clone(),
                FileDiff {
                    path,
                    status,
                    additions: additions as u32,
                    deletions: deletions as u32,
                    patch: patch_str,
                    truncated,
                },
            );
        }

        let mut out: Vec<FileDiff> = entries.into_values().collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    })
    .await
    .map_err(|e| format!("diff join error: {e}"))?
}

// ── Stats helpers ───────────────────────────────────────────────────────

#[derive(Default)]
struct CommitStats {
    files_changed: u32,
    insertions: u32,
    deletions: u32,
    changed_files: Vec<String>,
}

fn diff_between(repo: &Repository, parent: Option<Oid>, child: Oid) -> Option<git2::Diff<'_>> {
    let child_tree = repo.find_commit(child).ok()?.tree().ok()?;
    let parent_tree = match parent {
        Some(p) => repo.find_commit(p).ok()?.tree().ok(),
        None => None,
    };
    repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&child_tree), None).ok()
}

fn compute_stats(repo: &Repository, parent: Option<Oid>, child: Oid) -> CommitStats {
    let mut out = CommitStats::default();
    let Some(diff) = diff_between(repo, parent, child) else {
        return out;
    };
    if let Ok(stats) = diff.stats() {
        out.files_changed = stats.files_changed() as u32;
        out.insertions = stats.insertions() as u32;
        out.deletions = stats.deletions() as u32;
    }
    // Cap path list at 50 to bound payload; `files_changed` is the
    // authoritative total, so callers compute overflow as
    // `files_changed - changed_files.len()`.
    let _ = diff.foreach(
        &mut |delta, _| {
            if out.changed_files.len() >= 50 {
                return true;
            }
            let p = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().to_string());
            if let Some(p) = p {
                out.changed_files.push(p);
            }
            true
        },
        None,
        None,
        None,
    );
    out
}

fn collect_workspace_files(base: &Path, current: &Path, out: &mut Vec<PathBuf>) {
    let rd = match std::fs::read_dir(current) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if EXCLUDE_DIRS.iter().any(|x| *x == name_str.as_ref()) {
                continue;
            }
            collect_workspace_files(base, &path, out);
        } else if ft.is_file() {
            let m = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if m.len() > MAX_FILE_BYTES {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

// ── Garbage collection ──────────────────────────────────────────────────

pub const DEFAULT_GC_RETENTION_DAYS: u64 = 30;

/// Delete every `refs/gloss/...` ref pointing to a commit older
/// than `max_age_secs`. Returns the number of refs removed.
pub async fn gc_stale_refs(workspace: &Path, max_age_secs: u64) -> Result<u32, String> {
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<u32, String> {
        let repo = open_or_init(&workspace)?;
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now_secs.saturating_sub(max_age_secs);

        let to_remove: Vec<String> = {
            let refs = repo
                .references_glob(&format!("{REF_PREFIX}*"))
                .map_err(|e| format!("references_glob: {e}"))?;
            refs.flatten()
                .filter_map(|r| {
                    let name = r.name()?.to_string();
                    let target = r.target()?;
                    let commit = repo.find_commit(target).ok()?;
                    let when = commit.time().seconds() as u64;
                    // `<=`: with `max_age_secs == 0` we want to prune
                    // everything not strictly in the future. Commits
                    // born in the same second as the GC pass count as
                    // "at the cutoff" and are eligible.
                    if when <= cutoff {
                        Some(name)
                    } else {
                        None
                    }
                })
                .collect()
        };

        let mut deleted = 0u32;
        for name in to_remove {
            if let Ok(mut r) = repo.find_reference(&name) {
                if r.delete().is_ok() {
                    deleted += 1;
                }
            }
        }
        Ok(deleted)
    })
    .await
    .map_err(|e| format!("gc join error: {e}"))?
}

/// Best-effort, debounced GC. Runs `gc_stale_refs` at most once per 24h
/// per workspace, gated by the mtime of `<repo_dir>/.last_gc`. Cheap to
/// call from hot paths — typical case is a single `metadata()` syscall
/// and an early return.
pub async fn maybe_gc(workspace: &Path) {
    let dir = repo_dir(workspace);
    let marker = dir.join(".last_gc");

    let should_run = match std::fs::metadata(&marker).and_then(|m| m.modified()) {
        Ok(modified) => std::time::SystemTime::now()
            .duration_since(modified)
            .map(|e| e.as_secs() >= 24 * 60 * 60)
            .unwrap_or(true),
        Err(_) => true,
    };
    if !should_run {
        return;
    }

    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&marker, b"");

    if let Ok(removed) = gc_stale_refs(workspace, DEFAULT_GC_RETENTION_DAYS * 24 * 60 * 60).await {
        if removed > 0 {
            log::info!(
                "gloss gc: pruned {removed} refs older than {DEFAULT_GC_RETENTION_DAYS}d"
            );
        }
    }
}
