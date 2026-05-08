//! Hand-rolled stdio MCP server for gloss.
//!
//! Speaks JSON-RPC 2.0 over line-delimited stdin/stdout. Implements the
//! minimum subset of the MCP protocol needed by Claude Code / Claude
//! Desktop / Cursor: `initialize`, `tools/list`, `tools/call`. No
//! resources, no prompts — those aren't relevant to this product.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::{diff, list, restore, snapshot, web, FileStatus};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "gloss";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

type RunningUis = Arc<Mutex<HashMap<String, u16>>>;

pub async fn run(workspace: PathBuf) -> Result<(), String> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let running: RunningUis = Arc::new(Mutex::new(HashMap::new()));

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[gloss mcp] bad json: {e}");
                continue;
            }
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Notifications (no id) get no response.
        if id.is_none() {
            continue;
        }

        let result = handle_method(method, &req, &workspace, &running).await;
        let response = match result {
            Ok(value) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": value,
            }),
            Err((code, message)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            }),
        };

        let s = serde_json::to_string(&response).unwrap_or_default();
        if stdout.write_all(s.as_bytes()).await.is_err() {
            break;
        }
        let _ = stdout.write_all(b"\n").await;
        let _ = stdout.flush().await;
    }
    Ok(())
}

async fn handle_method(
    method: &str,
    req: &Value,
    workspace: &Path,
    running: &RunningUis,
) -> Result<Value, (i32, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        })),
        "tools/list" => Ok(json!({ "tools": tools_manifest() })),
        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or((-32602, "missing tool name".into()))?;
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            call_tool(name, &args, workspace, running).await
        }
        "ping" => Ok(json!({})),
        _ => Err((-32601, format!("method not found: {method}"))),
    }
}

fn tools_manifest() -> Vec<Value> {
    vec![
        json!({
            "name": "snap",
            "description": "Take a gloss checkpoint of the current workspace. Use this BEFORE any risky multi-file edit, refactor, or unfamiliar task. The user can later browse all snapshots in the review UI and roll back if anything goes wrong. Returns the checkpoint id and a count of files captured. Cheap and non-destructive — does not touch the user's real .git.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Short human-readable description of what is about to happen, e.g. 'before refactoring auth middleware'."
                    }
                }
            }
        }),
        json!({
            "name": "list",
            "description": "List recent gloss checkpoints for the current workspace, newest first. Each entry has id, label, age, files_changed, insertions, deletions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 20 }
                }
            }
        }),
        json!({
            "name": "diff",
            "description": "Show the diff captured by a checkpoint (id may be a unique prefix). Omit `id` to see the most recent checkpoint — useful for 'show the user what I just changed'. Patches are truncated at 64KiB per file. Prefer this over running `git diff` when the user asks 'what did you change'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Checkpoint id or unique prefix. Omit for the latest checkpoint." }
                }
            }
        }),
        json!({
            "name": "restore",
            "description": "Restore the workspace to a checkpoint. DESTRUCTIVE: overwrites unsaved changes in the working tree (a stash ref is captured first as a safety net, but the working tree DOES change). MUST be confirmed by the human user — do NOT call this without an explicit user instruction. The `confirm` parameter is a guard: it must be true AND the human must have asked for restore in this turn.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Checkpoint id or unique prefix." },
                    "confirm": { "type": "boolean", "description": "Must be true. The human user must have explicitly asked to restore in this turn." }
                },
                "required": ["id", "confirm"]
            }
        }),
        json!({
            "name": "start_review",
            "description": "Start a browser-based review UI for the current workspace and return its URL. Use this when the user says 'open the review', 'show me the diffs in browser', '启动 review', or similar. Each `label` gets its own deterministic port so multiple Claude sessions can run side by side without colliding. Idempotent — calling twice with the same label returns the same URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Session label. Becomes the browser tab title and badge. Pick something descriptive of the current task."
                    }
                }
            }
        }),
    ]
}

async fn call_tool(
    name: &str,
    args: &Value,
    workspace: &Path,
    running: &RunningUis,
) -> Result<Value, (i32, String)> {
    match name {
        "snap" => {
            let label = args.get("label").and_then(|v| v.as_str()).map(String::from);
            let cp = snapshot(workspace, label, None)
                .await
                .map_err(|e| (-32000, e))?;
            let text = format!(
                "checkpoint {} captured\n  files: {}  +{} -{}{}",
                cp.id,
                cp.files_changed,
                cp.insertions,
                cp.deletions,
                cp.label
                    .as_deref()
                    .map(|l| format!("\n  label: {l}"))
                    .unwrap_or_default(),
            );
            Ok(text_result(&text))
        }
        "list" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            let items = list(workspace);
            if items.is_empty() {
                return Ok(text_result("(no checkpoints yet)"));
            }
            let mut out = String::new();
            for cp in items.iter().take(limit) {
                let label = cp.label.as_deref().unwrap_or("(auto)");
                out.push_str(&format!(
                    "{}  {:>3} files  +{:<5} -{:<5}  {}\n",
                    cp.id, cp.files_changed, cp.insertions, cp.deletions, label
                ));
            }
            Ok(text_result(out.trim_end()))
        }
        "diff" => {
            let target = match args.get("id").and_then(|v| v.as_str()) {
                Some(prefix) => resolve_id(workspace, prefix)?,
                None => list(workspace)
                    .into_iter()
                    .next()
                    .map(|c| c.id)
                    .ok_or((-32000, "no checkpoints yet".into()))?,
            };
            let diffs = diff(workspace, &target)
                .await
                .map_err(|e| (-32000, e))?;
            if diffs.is_empty() {
                return Ok(text_result(&format!("checkpoint {target} has no changes")));
            }
            let mut out = format!("checkpoint {target} ({} files)\n\n", diffs.len());
            for fd in &diffs {
                let status = match fd.status {
                    FileStatus::Added => "added",
                    FileStatus::Modified => "modified",
                    FileStatus::Deleted => "deleted",
                    FileStatus::Renamed => "renamed",
                    FileStatus::Copied => "copied",
                };
                out.push_str(&format!(
                    "── {} ── ({}, +{} -{}){}\n",
                    fd.path,
                    status,
                    fd.additions,
                    fd.deletions,
                    if fd.truncated { " [TRUNCATED]" } else { "" }
                ));
                if fd.patch.is_empty() {
                    out.push_str("(binary or rename-only)\n\n");
                } else {
                    out.push_str(&fd.patch);
                    if !fd.patch.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push('\n');
                }
            }
            Ok(text_result(out.trim_end()))
        }
        "restore" => {
            let confirm = args.get("confirm").and_then(|v| v.as_bool()).unwrap_or(false);
            if !confirm {
                return Err((
                    -32000,
                    "refusing to restore: `confirm` must be true and the human user must have explicitly asked".into(),
                ));
            }
            let prefix = args
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or((-32602, "missing `id`".into()))?;
            let target = resolve_id(workspace, prefix)?;
            let report = restore(workspace, &target, None)
                .await
                .map_err(|e| (-32000, e))?;
            let mut text = format!(
                "restored {} files, removed {} files",
                report.restored_files.len(),
                report.removed_files.len()
            );
            if let Some(stash) = report.stash_commit {
                text.push_str(&format!(
                    "\npre-restore working tree captured at stash commit {}",
                    &stash[..stash.len().min(8)]
                ));
            }
            Ok(text_result(&text))
        }
        "start_review" => {
            let label = args.get("label").and_then(|v| v.as_str()).map(String::from);
            let port = web::port_for(workspace, label.as_deref());
            let key = label.clone().unwrap_or_default();
            let mut map = running.lock().await;
            if !map.contains_key(&key) {
                let ws = workspace.to_path_buf();
                let lbl = label.clone();
                tokio::spawn(async move {
                    if let Err(e) = web::serve(ws, port, lbl).await {
                        eprintln!("[gloss mcp] review ui exited: {e}");
                    }
                });
                map.insert(key, port);
                // Give the server a moment to bind so the URL works when Claude prints it.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            let url = format!("http://127.0.0.1:{port}");
            spawn_browser(&url);
            let banner = match label.as_deref() {
                Some(l) => format!("review UI for `{l}` is open at {url}"),
                None => format!("review UI is open at {url}"),
            };
            Ok(text_result(&banner))
        }
        other => Err((-32601, format!("unknown tool: {other}"))),
    }
}

fn resolve_id(workspace: &Path, prefix: &str) -> Result<String, (i32, String)> {
    let items = list(workspace);
    let matches: Vec<_> = items.iter().filter(|c| c.id.starts_with(prefix)).collect();
    match matches.len() {
        0 => Err((-32000, format!("no checkpoint matches `{prefix}`"))),
        1 => Ok(matches[0].id.clone()),
        n => Err((-32000, format!("ambiguous prefix `{prefix}` matches {n} checkpoints"))),
    }
}

fn text_result(text: &str) -> Value {
    json!({
        "content": [
            { "type": "text", "text": text }
        ]
    })
}

fn spawn_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let url = url.to_string();
    std::thread::spawn(move || {
        let _ = std::process::Command::new(cmd).arg(&url).spawn();
    });
}
