//! `gloss install` / `uninstall` — wire gloss into Claude Code:
//!   1. register the MCP server (via `claude mcp add`)
//!   2. add SessionStart + UserPromptSubmit hooks to ~/.claude/settings.json
//!   3. install the /gloss slash command
//!
//! All steps are idempotent: re-running `install` is a no-op for parts
//! that are already in place. `uninstall` reverses each step.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

const SESSION_HOOK_CMD: &str =
    "(gloss review \"$(basename \"$PWD\")\" >/dev/null 2>&1 &) || true";
const PROMPT_HOOK_CMD: &str =
    "gloss snap \"turn-$(date +%H%M%S)\" >/dev/null 2>&1 || true";
const SLASH_CMD_BODY: &str = r#"---
argument-hint: [label]
description: Open the gloss review browser tab for the current workspace.
allowed-tools: Bash
---

Run the following command using the Bash tool with `run_in_background: true`:

```
gloss review "$ARGUMENTS"
```

If `$ARGUMENTS` is empty, omit it — `gloss review` will pick a label
automatically from the workspace path.

The command:
- Computes a deterministic port from (workspace, label) so the same project +
  label always opens the same URL.
- If a server is already running on that port, just opens the browser
  (idempotent).
- Otherwise starts the server and opens the browser.

After running, briefly tell the user the URL.
"#;

#[derive(Default)]
pub struct InstallReport {
    pub mcp: StepStatus,
    pub session_hook: StepStatus,
    pub prompt_hook: StepStatus,
    pub slash_command: StepStatus,
}

#[derive(Default)]
pub enum StepStatus {
    #[default]
    Skipped,
    Added,
    Failed(String),
}

impl StepStatus {
    fn icon(&self) -> &'static str {
        match self {
            Self::Added => "✓",
            Self::Skipped => "·",
            Self::Failed(_) => "✗",
        }
    }
    fn note(&self) -> String {
        match self {
            Self::Added => "added".into(),
            Self::Skipped => "already present".into(),
            Self::Failed(e) => format!("failed: {e}"),
        }
    }
}

pub fn install() -> Result<InstallReport, String> {
    let home = dirs::home_dir().ok_or("cannot find home dir")?;
    let mut report = InstallReport::default();

    report.mcp = match register_mcp() {
        Ok(true) => StepStatus::Added,
        Ok(false) => StepStatus::Skipped,
        Err(e) => StepStatus::Failed(e),
    };

    let settings_path = home.join(".claude/settings.json");
    match ensure_hooks(&settings_path) {
        Ok((s, p)) => {
            report.session_hook = if s { StepStatus::Added } else { StepStatus::Skipped };
            report.prompt_hook = if p { StepStatus::Added } else { StepStatus::Skipped };
        }
        Err(e) => {
            report.session_hook = StepStatus::Failed(e.clone());
            report.prompt_hook = StepStatus::Failed(e);
        }
    }

    let cmd_path = home.join(".claude/commands/gloss.md");
    report.slash_command = match write_slash_command(&cmd_path) {
        Ok(true) => StepStatus::Added,
        Ok(false) => StepStatus::Skipped,
        Err(e) => StepStatus::Failed(e),
    };

    Ok(report)
}

pub fn uninstall() -> Result<InstallReport, String> {
    let home = dirs::home_dir().ok_or("cannot find home dir")?;
    let mut report = InstallReport::default();

    report.mcp = match unregister_mcp() {
        Ok(true) => StepStatus::Added, // "Added" reused as "did the work"
        Ok(false) => StepStatus::Skipped,
        Err(e) => StepStatus::Failed(e),
    };

    let settings_path = home.join(".claude/settings.json");
    match remove_hooks(&settings_path) {
        Ok((s, p)) => {
            report.session_hook = if s { StepStatus::Added } else { StepStatus::Skipped };
            report.prompt_hook = if p { StepStatus::Added } else { StepStatus::Skipped };
        }
        Err(e) => {
            report.session_hook = StepStatus::Failed(e.clone());
            report.prompt_hook = StepStatus::Failed(e);
        }
    }

    let cmd_path = home.join(".claude/commands/gloss.md");
    report.slash_command = if cmd_path.exists() {
        match std::fs::remove_file(&cmd_path) {
            Ok(_) => StepStatus::Added,
            Err(e) => StepStatus::Failed(e.to_string()),
        }
    } else {
        StepStatus::Skipped
    };

    Ok(report)
}

pub fn print_report(label: &str, report: &InstallReport) {
    println!("gloss {label}:");
    let rows = [
        ("MCP server", &report.mcp),
        ("SessionStart hook", &report.session_hook),
        ("UserPromptSubmit hook", &report.prompt_hook),
        ("/gloss command", &report.slash_command),
    ];
    let action_done = if label == "uninstall" { "removed" } else { "added" };
    let action_skip = if label == "uninstall" { "not present" } else { "already present" };
    for (name, s) in rows {
        let note = match s {
            StepStatus::Added => action_done.to_string(),
            StepStatus::Skipped => action_skip.to_string(),
            StepStatus::Failed(e) => format!("failed: {e}"),
        };
        println!("  {} {:<26} {}", s.icon(), name, note);
    }
    println!("\nrestart Claude Code for changes to take effect.");
}

// ── MCP registration ───────────────────────────────────────────────────

fn register_mcp() -> Result<bool, String> {
    if !claude_cli_present() {
        return Err("`claude` CLI not on PATH; run `claude mcp add --scope user gloss -- gloss mcp` manually".into());
    }
    if mcp_listed("gloss")? {
        return Ok(false);
    }
    let status = Command::new("claude")
        .args(["mcp", "add", "--scope", "user", "gloss", "--", "gloss", "mcp"])
        .status()
        .map_err(|e| format!("spawn claude: {e}"))?;
    if !status.success() {
        return Err(format!("`claude mcp add` exited {status}"));
    }
    Ok(true)
}

fn unregister_mcp() -> Result<bool, String> {
    if !claude_cli_present() {
        return Err("`claude` CLI not on PATH".into());
    }
    if !mcp_listed("gloss")? {
        return Ok(false);
    }
    let status = Command::new("claude")
        .args(["mcp", "remove", "gloss"])
        .status()
        .map_err(|e| format!("spawn claude: {e}"))?;
    if !status.success() {
        return Err(format!("`claude mcp remove` exited {status}"));
    }
    Ok(true)
}

fn claude_cli_present() -> bool {
    Command::new("claude")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn mcp_listed(name: &str) -> Result<bool, String> {
    let out = Command::new("claude")
        .args(["mcp", "list"])
        .output()
        .map_err(|e| format!("spawn claude: {e}"))?;
    let s = String::from_utf8_lossy(&out.stdout);
    Ok(s.lines().any(|l| l.starts_with(&format!("{name}:"))))
}

// ── settings.json hooks ────────────────────────────────────────────────

fn ensure_hooks(path: &Path) -> Result<(bool, bool), String> {
    let mut value = read_json_or_empty(path)?;
    let hooks = value
        .as_object_mut()
        .ok_or_else(|| "settings.json root is not an object".to_string())?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| "settings.json `hooks` is not an object".to_string())?;

    let session_added = ensure_hook(hooks, "SessionStart", SESSION_HOOK_CMD, "gloss review");
    let prompt_added = ensure_hook(hooks, "UserPromptSubmit", PROMPT_HOOK_CMD, "gloss snap");

    write_json(path, &value)?;
    Ok((session_added, prompt_added))
}

fn remove_hooks(path: &Path) -> Result<(bool, bool), String> {
    if !path.exists() {
        return Ok((false, false));
    }
    let mut value = read_json_or_empty(path)?;
    let Some(hooks) = value
        .as_object_mut()
        .and_then(|m| m.get_mut("hooks"))
        .and_then(|h| h.as_object_mut())
    else {
        return Ok((false, false));
    };

    let session_removed = strip_hook(hooks, "SessionStart", "gloss review");
    let prompt_removed = strip_hook(hooks, "UserPromptSubmit", "gloss snap");

    write_json(path, &value)?;
    Ok((session_removed, prompt_removed))
}

/// Append our hook to `hooks[event]` if no inner command already contains
/// `marker`. Adds as a fresh top-level entry rather than merging into an
/// existing entry's inner array — keeps uninstall simple.
fn ensure_hook(
    hooks: &mut serde_json::Map<String, Value>,
    event: &str,
    cmd: &str,
    marker: &str,
) -> bool {
    let arr = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("just inserted array");

    if hook_contains(arr, marker) {
        return false;
    }
    arr.push(json!({
        "hooks": [
            { "type": "command", "command": cmd }
        ]
    }));
    true
}

fn hook_contains(arr: &[Value], marker: &str) -> bool {
    for entry in arr {
        let Some(inner) = entry.get("hooks").and_then(|v| v.as_array()) else { continue };
        for h in inner {
            if h.get("command")
                .and_then(|v| v.as_str())
                .map(|c| c.contains(marker))
                .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

/// Remove every inner command containing `marker`. If an outer entry's
/// `hooks` array becomes empty, drop that outer entry too.
fn strip_hook(
    hooks: &mut serde_json::Map<String, Value>,
    event: &str,
    marker: &str,
) -> bool {
    let Some(arr) = hooks.get_mut(event).and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut removed = false;
    arr.retain_mut(|entry| {
        let Some(inner) = entry.get_mut("hooks").and_then(|v| v.as_array_mut()) else {
            return true;
        };
        let before = inner.len();
        inner.retain(|h| {
            !h.get("command")
                .and_then(|v| v.as_str())
                .map(|c| c.contains(marker))
                .unwrap_or(false)
        });
        if inner.len() < before {
            removed = true;
        }
        !inner.is_empty()
    });
    if arr.is_empty() {
        hooks.remove(event);
    }
    removed
}

// ── slash command ──────────────────────────────────────────────────────

fn write_slash_command(path: &Path) -> Result<bool, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    if path.exists() {
        if let Ok(existing) = std::fs::read_to_string(path) {
            if existing == SLASH_CMD_BODY {
                return Ok(false);
            }
        }
    }
    std::fs::write(path, SLASH_CMD_BODY).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(true)
}

// ── json io ────────────────────────────────────────────────────────────

fn read_json_or_empty(path: &Path) -> Result<Value, String> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        return Ok(json!({}));
    }
    let s = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if s.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&s).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let s = serde_json::to_string_pretty(value).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(path, s).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}
