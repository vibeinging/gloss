//! Integration tests for the checkpoint engine.
//!
//! All tests are `#[serial]` because they share the `GLOSS_HOME`
//! env var. SQLite-style WAL contention isn't an issue here, but env
//! var contention between parallel tests certainly is.

use serial_test::serial;
use gloss::{diff, gc_stale_refs, list, restore, snapshot};
use std::collections::HashSet;
use std::path::PathBuf;

fn temp_root() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("gloss_test_{}", uuid::Uuid::new_v4()));
    p
}

#[tokio::test]
#[serial]
async fn snapshot_then_restore_roundtrips_file_contents() {
    let tmp = temp_root();
    std::env::set_var("GLOSS_HOME", &tmp);
    let workspace = tmp.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("a.txt"), "hello").unwrap();
    std::fs::create_dir_all(workspace.join("sub")).unwrap();
    std::fs::write(workspace.join("sub/b.txt"), "world").unwrap();

    let cp = snapshot(&workspace, Some("init".into()), None).await.unwrap();

    std::fs::write(workspace.join("a.txt"), "MUTATED").unwrap();
    std::fs::write(workspace.join("c.txt"), "added").unwrap();
    std::fs::remove_file(workspace.join("sub/b.txt")).unwrap();

    let report = restore(&workspace, &cp.id, None).await.unwrap();
    assert!(report.restored_files.iter().any(|f| f == "a.txt"));
    assert!(report.restored_files.iter().any(|f| f.ends_with("b.txt")));

    assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "hello");
    assert_eq!(std::fs::read_to_string(workspace.join("sub/b.txt")).unwrap(), "world");
    assert!(!workspace.join("c.txt").exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
#[serial]
async fn list_returns_newest_first_with_labels() {
    let tmp = temp_root();
    std::env::set_var("GLOSS_HOME", &tmp);
    let workspace = tmp.join("ws_list");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("x"), "1").unwrap();

    let _a = snapshot(&workspace, Some("first".into()), None).await.unwrap();
    std::fs::write(workspace.join("x"), "2").unwrap();
    // Tiny sleep so the two ids fall in distinct ms; reliable on macOS/Linux.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let b = snapshot(&workspace, Some("second".into()), None).await.unwrap();

    let snaps = list(&workspace);
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].id, b.id, "newest must be first");
    assert_eq!(snaps[0].label.as_deref(), Some("second"));
    assert_eq!(snaps[1].label.as_deref(), Some("first"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
#[serial]
async fn incremental_snapshot_preserves_unrelated_files() {
    let tmp = temp_root();
    std::env::set_var("GLOSS_HOME", &tmp);
    let workspace = tmp.join("ws_inc");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("touched.txt"), "v1").unwrap();
    std::fs::write(workspace.join("untouched.txt"), "STAYS").unwrap();
    std::fs::create_dir_all(workspace.join("sub")).unwrap();
    std::fs::write(workspace.join("sub/deep.txt"), "DEEP").unwrap();

    snapshot(&workspace, None, None).await.unwrap();

    std::fs::write(workspace.join("touched.txt"), "v2").unwrap();
    let mut dirty = HashSet::new();
    dirty.insert(PathBuf::from("touched.txt"));
    let cp = snapshot(&workspace, None, Some(dirty)).await.unwrap();

    std::fs::write(workspace.join("touched.txt"), "AFTER").unwrap();
    std::fs::write(workspace.join("untouched.txt"), "AFTER").unwrap();
    std::fs::write(workspace.join("sub/deep.txt"), "AFTER").unwrap();

    restore(&workspace, &cp.id, None).await.unwrap();
    assert_eq!(std::fs::read_to_string(workspace.join("touched.txt")).unwrap(), "v2");
    assert_eq!(std::fs::read_to_string(workspace.join("untouched.txt")).unwrap(), "STAYS");
    assert_eq!(std::fs::read_to_string(workspace.join("sub/deep.txt")).unwrap(), "DEEP");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
#[serial]
async fn excluded_dirs_survive_restore() {
    let tmp = temp_root();
    std::env::set_var("GLOSS_HOME", &tmp);
    let workspace = tmp.join("ws_excl");
    std::fs::create_dir_all(workspace.join("node_modules/junk")).unwrap();
    std::fs::write(workspace.join("node_modules/junk/big.bin"), vec![0u8; 1024]).unwrap();
    std::fs::write(workspace.join("keep.txt"), "yes").unwrap();

    let cp = snapshot(&workspace, None, None).await.unwrap();

    std::fs::write(workspace.join("keep.txt"), "no").unwrap();
    std::fs::write(workspace.join("node_modules/junk/sentinel"), "alive").unwrap();

    restore(&workspace, &cp.id, None).await.unwrap();

    assert_eq!(std::fs::read_to_string(workspace.join("keep.txt")).unwrap(), "yes");
    assert!(workspace.join("node_modules/junk/sentinel").exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
#[serial]
async fn diff_returns_unified_patch_per_file() {
    let tmp = temp_root();
    std::env::set_var("GLOSS_HOME", &tmp);
    let workspace = tmp.join("ws_diff");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("a.txt"), "alpha\n").unwrap();
    snapshot(&workspace, None, None).await.unwrap();

    std::fs::write(workspace.join("a.txt"), "alpha\nbeta\n").unwrap();
    let cp = snapshot(&workspace, None, None).await.unwrap();

    let diffs = diff(&workspace, &cp.id).await.unwrap();
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "a.txt");
    assert!(diffs[0].patch.contains("+beta"));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
#[serial]
async fn restore_stashes_uncommitted_handedits() {
    let tmp = temp_root();
    std::env::set_var("GLOSS_HOME", &tmp);
    let workspace = tmp.join("ws_stash");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("a.txt"), "v1").unwrap();
    let cp = snapshot(&workspace, None, None).await.unwrap();

    std::fs::write(workspace.join("a.txt"), "v2").unwrap();
    snapshot(&workspace, None, None).await.unwrap();

    // User hand-edits AFTER the latest snapshot — must survive as stash.
    std::fs::write(workspace.join("a.txt"), "user_edit").unwrap();
    std::fs::write(workspace.join("manual.txt"), "by_hand").unwrap();

    let report = restore(&workspace, &cp.id, None).await.unwrap();
    assert!(report.stash_commit.is_some(), "hand-edits should be stashed");
    assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "v1");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
#[serial]
async fn gc_prunes_only_old_commits() {
    let tmp = temp_root();
    std::env::set_var("GLOSS_HOME", &tmp);
    let workspace = tmp.join("ws_gc");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("a"), "x").unwrap();
    snapshot(&workspace, None, None).await.unwrap();
    snapshot(&workspace, None, None).await.unwrap();

    let removed = gc_stale_refs(&workspace, 0).await.unwrap();
    assert!(removed >= 2, "expected all refs pruned, got {removed}");
    assert!(list(&workspace).is_empty());

    std::fs::write(workspace.join("a"), "y").unwrap();
    snapshot(&workspace, None, None).await.unwrap();
    let removed_again = gc_stale_refs(&workspace, 3600).await.unwrap();
    assert_eq!(removed_again, 0, "fresh refs must survive GC");
    assert_eq!(list(&workspace).len(), 1);

    let _ = std::fs::remove_dir_all(&tmp);
}
